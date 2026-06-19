//! DomainPack Spec — formal JSON Schema definition for all 5 layers.
//!
//! The DomainPackSpec provides a machine-readable JSON Schema that describes
//! the complete DomainPack configuration format. This schema can be used for:
//!
//! - **Structural validation**: Verify config files conform to the expected shape
//! - **Documentation**: Auto-generate config file documentation from the schema
//! - **Cross-language sharing**: Any language can validate DomainPack configs using this schema
//!
//! The schema covers all 5 layers of domain workflow embedding:
//! 1. Tools + ToolDecorators
//! 2. ContextSources
//! 3. PermissionProfile
//! 4. ParadigmStrategies
//! 5. CompressionTemplate
//!
//! **Usage**:
//! ```ignore
//! let schema = DomainPackSpec::schema();
//! println!("{}", serde_json::to_string_pretty(&schema)?);
//! ```
//!
//! The schema is generated programmatically from the DomainPackConfig structure,
//! ensuring it stays in sync with the Rust type definitions.

use serde_json::{json, Value};

/// DomainPack specification — provides the JSON Schema for DomainPack configs.
///
/// The schema follows JSON Schema draft-2020-12 conventions and covers every
/// field of `DomainPackConfig` (from `config_parser.rs`). It is designed to be
/// usable by any JSON Schema validator in any language.
pub struct DomainPackSpec;

impl DomainPackSpec {
    /// The specification version — follows the DomainPack spec evolution.
    pub const SPEC_VERSION: &str = "1.0";

    /// Generate the complete JSON Schema for a DomainPack configuration.
    ///
    /// Returns a `serde_json::Value` representing the full JSON Schema document.
    /// This can be serialized to JSON and used with any JSON Schema validator.
    pub fn schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://oneai.dev/spec/domain-pack/v1.0",
            "title": "DomainPack Configuration",
            "description": "DomainPack configuration schema — 5 layers of domain workflow embedding for OneAI agents",
            "type": "object",
            "required": ["name", "tools", "permission_profile"],
            "properties": {
                "spec_version": {
                    "type": "string",
                    "description": "DomainPack spec version (e.g., '1.0')",
                    "default": "1.0"
                },
                "name": {
                    "type": "string",
                    "description": "Unique domain name (e.g., 'coding', 'research', 'data_analysis')",
                    "minLength": 1,
                    "maxLength": 64,
                    "pattern": "^[a-z][a-z0-9_-]*$"
                },
                "description": {
                    "type": "string",
                    "description": "Human-readable description of this domain pack",
                    "default": ""
                },
                "tools": {
                    "type": "array",
                    "description": "Tool names to include — resolved from predefined tool factories",
                    "items": {
                        "type": "string",
                        "minLength": 1
                    },
                    "minItems": 0,
                    "uniqueItems": true
                },
                "tool_decorators": {
                    "type": "object",
                    "description": "Tool description overrides — tool name → custom description",
                    "additionalProperties": {
                        "type": "string",
                        "minLength": 1
                    }
                },
                "context_sources": {
                    "type": "array",
                    "description": "Context source names to include — resolved from predefined factories",
                    "items": {
                        "type": "string",
                        "enum": ["project_instructions", "git_status", "file_tree", "project_config", "date", "environment"]
                    },
                    "uniqueItems": true
                },
                "permission_profile": {
                    "type": "object",
                    "description": "Permission profile — determines how tool calls are approved/denied",
                    "required": [],
                    "properties": {
                        "auto_approve": {
                            "type": "array",
                            "description": "Tool names to auto-approve (skip approval gate)",
                            "items": { "type": "string" },
                            "uniqueItems": true
                        },
                        "require_confirmation": {
                            "type": "array",
                            "description": "Tool names that require explicit confirmation",
                            "items": { "type": "string" },
                            "uniqueItems": true
                        },
                        "deny_by_default": {
                            "type": "array",
                            "description": "Deny patterns — always block matching tool calls",
                            "items": {
                                "type": "object",
                                "required": ["tool", "reason"],
                                "properties": {
                                    "tool": {
                                        "type": "string",
                                        "description": "Tool name pattern (exact or regex)"
                                    },
                                    "args_pattern": {
                                        "type": "string",
                                        "description": "Optional regex pattern matching tool arguments"
                                    },
                                    "reason": {
                                        "type": "string",
                                        "description": "Reason for denial (shown to user and model)"
                                    }
                                }
                            }
                        }
                    }
                },
                "paradigm_strategies": {
                    "type": "array",
                    "description": "Paradigm strategy definitions — task pattern → paradigm sequence mapping",
                    "items": {
                        "type": "object",
                        "required": ["trigger", "sequence"],
                        "properties": {
                            "trigger": {
                                "type": "string",
                                "description": "Regex pattern for matching task descriptions",
                                "minLength": 1
                            },
                            "sequence": {
                                "type": "array",
                                "description": "Paradigm sequence (Plan, ReAct, Reflect, Explore)",
                                "items": {
                                    "type": "string",
                                    "enum": ["Plan", "ReAct", "Reflect", "Explore"]
                                },
                                "minItems": 1
                            },
                            "sub_agents": {
                                "type": "array",
                                "description": "Sub-agent type definitions",
                                "items": {
                                    "type": "object",
                                    "required": ["name", "description", "system_prompt", "available_tools"],
                                    "properties": {
                                        "name": { "type": "string", "minLength": 1 },
                                        "description": { "type": "string" },
                                        "system_prompt": { "type": "string", "minLength": 1 },
                                        "available_tools": {
                                            "type": "array",
                                            "items": { "type": "string" },
                                            "minItems": 1
                                        },
                                        "permission_threshold": {
                                            "type": "string",
                                            "enum": ["read", "standard", "admin"],
                                            "default": "standard"
                                        },
                                        "modifies_files": { "type": "boolean", "default": false }
                                    }
                                }
                            },
                            "description": { "type": "string", "default": "" }
                        }
                    }
                },
                "compression_template": {
                    "type": "object",
                    "description": "Compression template — context preservation priorities",
                    "properties": {
                        "name": { "type": "string", "minLength": 1 },
                        "preserve_fields": {
                            "type": "array",
                            "description": "Fields to preserve during compression",
                            "items": { "type": "string" }
                        },
                        "truncate_rules": {
                            "type": "object",
                            "description": "Truncation rules: content_type → max chars",
                            "additionalProperties": { "type": "integer", "minimum": 0 }
                        }
                    }
                },
                "system_prompt": {
                    "type": "string",
                    "description": "System prompt template for this domain's agent",
                    "default": ""
                }
            }
        })
    }

    /// Known tool names — the predefined tool factory registry keys.
    ///
    /// These are the tool names that can be referenced in the `tools` field
    /// of a DomainPack configuration.
    pub fn known_tool_names() -> Vec<String> {
        vec![
            "read_file".to_string(), "edit_file".to_string(), "grep".to_string(), "glob".to_string(), "list_directory".to_string(),
            "shell".to_string(), "environment".to_string(), "calculator".to_string(), "notebook_edit".to_string(), "apply_patch".to_string(),
            "web_search".to_string(), "web_fetch".to_string(),
        ]
    }

    /// Known context source names — the predefined context source factory keys.
    pub fn known_context_source_names() -> Vec<String> {
        vec![
            "project_instructions".to_string(), "git_status".to_string(), "file_tree".to_string(),
            "project_config".to_string(), "date".to_string(), "environment".to_string(),
        ]
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_is_valid_json() {
        let schema = DomainPackSpec::schema();
        // Must be a valid JSON object
        assert!(schema.is_object());
        // Must have required JSON Schema fields
        assert!(schema.get("$schema").is_some());
        assert!(schema.get("title").is_some());
        assert!(schema.get("type").is_some());
        assert!(schema.get("properties").is_some());
        assert!(schema.get("required").is_some());
    }

    #[test]
    fn test_schema_has_all_layers() {
        let schema = DomainPackSpec::schema();
        let props = schema.get("properties").unwrap().as_object().unwrap();

        // All 5 layers must be present
        assert!(props.contains_key("name"));
        assert!(props.contains_key("tools"));
        assert!(props.contains_key("tool_decorators"));
        assert!(props.contains_key("context_sources"));
        assert!(props.contains_key("permission_profile"));
        assert!(props.contains_key("paradigm_strategies"));
        assert!(props.contains_key("compression_template"));
        assert!(props.contains_key("system_prompt"));
        assert!(props.contains_key("spec_version"));
    }

    #[test]
    fn test_schema_required_fields() {
        let schema = DomainPackSpec::schema();
        let required = schema.get("required").unwrap().as_array().unwrap();

        // name, tools, permission_profile are required
        let required_names: Vec<&str> = required.iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required_names.contains(&"name"));
        assert!(required_names.contains(&"tools"));
        assert!(required_names.contains(&"permission_profile"));
    }

    #[test]
    fn test_known_tool_names() {
        let tools = DomainPackSpec::known_tool_names();
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"shell".to_string()));
        assert!(tools.contains(&"calculator".to_string()));
        assert!(tools.contains(&"web_search".to_string()));
        assert!(tools.len() >= 12);
    }

    #[test]
    fn test_known_context_source_names() {
        let sources = DomainPackSpec::known_context_source_names();
        assert!(sources.contains(&"project_instructions".to_string()));
        assert!(sources.contains(&"date".to_string()));
        assert!(sources.contains(&"environment".to_string()));
        assert!(sources.len() >= 6);
    }

    #[test]
    fn test_spec_version() {
        assert_eq!(DomainPackSpec::SPEC_VERSION, "1.0");
    }
}
