//! DomainPack Validator — structural (JSON Schema) + semantic (cross-layer) validation.
//!
//! The validator checks DomainPack configurations for:
//!
//! 1. **Structural validation**: Required fields present, correct types, valid enum values
//! 2. **Semantic validation**: Cross-layer consistency (tool references exist, trigger patterns are valid regex, etc.)
//!
//! The validation pipeline is:
//! ```text
//! raw file → DomainPackConfig (serde) → validator → ValidationResult → if valid, resolve_config() → DomainPack
//! ```
//!
//! **ValidationResult** contains:
//! - `is_valid`: true if zero Error-level issues (Warnings don't block)
//! - `issues`: list of ValidationIssue with severity (Error/Warning/Info)
//!
//! **Usage**:
//! ```ignore
//! let config = DomainPackConfig { ... };
//! let result = DomainPackValidator::validate(&config);
//! if result.is_valid() {
//!     let pack = resolve_config(&config, "/project");
//! } else {
//!     for issue in result.issues() {
//!         println!("{}: {} — {}", issue.severity, issue.layer, issue.message);
//!     }
//! }
//! ```

use std::collections::HashSet;

use crate::config_parser::DomainPackConfig;
use crate::spec::DomainPackSpec;

// ─── Validation Types ────────────────────────────────────────────────────────────

/// Validation severity — determines whether an issue blocks config resolution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ValidationSeverity {
    /// Blocks config resolution — must fix before building DomainPack.
    Error,
    /// Should fix — won't block resolution but indicates a potential problem.
    Warning,
    /// Informational — good practice suggestion, no action required.
    Info,
}

impl std::fmt::Display for ValidationSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationSeverity::Error => write!(f, "ERROR"),
            ValidationSeverity::Warning => write!(f, "WARNING"),
            ValidationSeverity::Info => write!(f, "INFO"),
        }
    }
}

/// A single validation issue found during DomainPack config validation.
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    /// Issue severity — Error blocks resolution, Warning does not.
    pub severity: ValidationSeverity,
    /// Which configuration layer the issue relates to.
    /// One of: "spec", "tools", "permissions", "strategies", "compression", "context", "general".
    pub layer: String,
    /// Human-readable description of the issue.
    pub message: String,
    /// Optional JSON path location (e.g., "permission_profile.auto_approve[2]").
    pub location: Option<String>,
}

impl ValidationIssue {
    /// Create an Error-level issue (blocks resolution).
    pub fn error(layer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: ValidationSeverity::Error,
            layer: layer.into(),
            message: message.into(),
            location: None,
        }
    }

    /// Create an Error-level issue with a specific location.
    pub fn error_at(layer: impl Into<String>, message: impl Into<String>, location: impl Into<String>) -> Self {
        Self {
            severity: ValidationSeverity::Error,
            layer: layer.into(),
            message: message.into(),
            location: Some(location.into()),
        }
    }

    /// Create a Warning-level issue (doesn't block resolution).
    pub fn warning(layer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: ValidationSeverity::Warning,
            layer: layer.into(),
            message: message.into(),
            location: None,
        }
    }

    /// Create an Info-level issue (informational).
    pub fn info(layer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: ValidationSeverity::Info,
            layer: layer.into(),
            message: message.into(),
            location: None,
        }
    }
}

/// Validation result — the outcome of validating a DomainPackConfig.
///
/// `is_valid()` returns true if there are zero Error-level issues.
/// Warning and Info issues are recorded but don't block resolution.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Whether the config is valid (zero Error-level issues).
    pub is_valid: bool,
    /// All issues found during validation.
    pub issues: Vec<ValidationIssue>,
}

impl ValidationResult {
    /// Create a valid result with no issues.
    pub fn valid() -> Self {
        Self { is_valid: true, issues: Vec::new() }
    }

    /// Create a result from a list of issues.
    ///
    /// `is_valid` is computed from the presence of Error-level issues.
    pub fn from_issues(issues: Vec<ValidationIssue>) -> Self {
        let is_valid = !issues.iter().any(|i| i.severity == ValidationSeverity::Error);
        Self { is_valid, issues }
    }

    /// Check if the result is valid (no Error-level issues).
    pub fn is_valid(&self) -> bool {
        self.is_valid
    }

    /// Get all issues.
    pub fn issues(&self) -> &[ValidationIssue] {
        &self.issues
    }

    /// Get only Error-level issues.
    pub fn errors(&self) -> Vec<&ValidationIssue> {
        self.issues.iter().filter(|i| i.severity == ValidationSeverity::Error).collect()
    }

    /// Get only Warning-level issues.
    pub fn warnings(&self) -> Vec<&ValidationIssue> {
        self.issues.iter().filter(|i| i.severity == ValidationSeverity::Warning).collect()
    }

    /// Get only Info-level issues.
    pub fn infos(&self) -> Vec<&ValidationIssue> {
        self.issues.iter().filter(|i| i.severity == ValidationSeverity::Info).collect()
    }
}

// ─── DomainPackValidator ──────────────────────────────────────────────────────────

/// DomainPack configuration validator — structural + semantic checks.
///
/// Validates a `DomainPackConfig` for correctness before resolution into
/// a `DomainPack`. The validator checks:
///
/// 1. **Structural**: Required fields, correct types, valid enum values
/// 2. **Semantic**: Cross-layer consistency, valid references, regex patterns
///
/// The validator uses the `DomainPackSpec` JSON Schema for structural checks
/// and a set of semantic rules for cross-layer consistency.
pub struct DomainPackValidator;

impl DomainPackValidator {
    /// Validate a DomainPackConfig — both structural and semantic checks.
    ///
    /// Returns a `ValidationResult` containing all issues found.
    /// `is_valid()` is true if zero Error-level issues.
    pub fn validate(config: &DomainPackConfig) -> ValidationResult {
        let mut issues = Vec::new();

        // Phase 1: Structural validation
        Self::validate_structural(config, &mut issues);

        // Phase 2: Semantic validation (cross-layer consistency)
        Self::validate_semantic(config, &mut issues);

        ValidationResult::from_issues(issues)
    }

    /// Validate only structural aspects (required fields, basic shape).
    fn validate_structural(config: &DomainPackConfig, issues: &mut Vec<ValidationIssue>) {
        // Name must be non-empty and match the pattern
        if config.name.is_empty() {
            issues.push(ValidationIssue::error("spec", "DomainPack name must not be empty"));
        } else if !config.name.chars().next().map(|c| c.is_ascii_lowercase()).unwrap_or(false) {
            issues.push(ValidationIssue::error("spec", "DomainPack name must start with a lowercase letter"));
        }

        // Name must match pattern [a-z][a-z0-9_-]*
        for ch in config.name.chars() {
            if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '_' && ch != '-' {
                issues.push(ValidationIssue::error_at(
                    "spec",
                    format!("DomainPack name contains invalid character '{}'", ch),
                    "name",
                ));
            }
        }

        // Tools must be an array (can be empty, but that's a warning)
        if config.tools.is_empty() {
            issues.push(ValidationIssue::warning("tools", "DomainPack has no tools — agent will have no capabilities"));
        }

        // Tool names must be non-empty strings
        for (i, tool_name) in config.tools.iter().enumerate() {
            if tool_name.is_empty() {
                issues.push(ValidationIssue::error_at(
                    "tools",
                    "Tool name must not be empty",
                    format!("tools[{}]", i),
                ));
            }
        }

        // Duplicate tool names
        let tool_set: HashSet<&str> = config.tools.iter().map(|s| s.as_str()).collect();
        if tool_set.len() != config.tools.len() {
            issues.push(ValidationIssue::error("tools", "Duplicate tool names in tools list"));
        }

        // Permission profile must have at least one rule
        if config.permission_profile.auto_approve.is_empty()
            && config.permission_profile.require_confirmation.is_empty()
            && config.permission_profile.deny_by_default.is_empty()
        {
            issues.push(ValidationIssue::warning("permissions", "Permission profile has no rules — all tools will require confirmation by default"));
        }

        // Paradigm strategies: trigger and sequence must be non-empty
        for (i, strategy) in config.paradigm_strategies.iter().enumerate() {
            if strategy.trigger.is_empty() {
                issues.push(ValidationIssue::error_at(
                    "strategies",
                    "Paradigm strategy trigger must not be empty",
                    format!("paradigm_strategies[{}].trigger", i),
                ));
            }
            if strategy.sequence.is_empty() {
                issues.push(ValidationIssue::error_at(
                    "strategies",
                    "Paradigm strategy sequence must have at least one paradigm",
                    format!("paradigm_strategies[{}].sequence", i),
                ));
            }
            // Sequence values must be valid paradigm names
            for (j, paradigm) in strategy.sequence.iter().enumerate() {
                if !["Plan", "ReAct", "Reflect", "Explore"].contains(&paradigm.as_str()) {
                    issues.push(ValidationIssue::error_at(
                        "strategies",
                        format!("Invalid paradigm name '{}' — must be one of Plan, ReAct, Reflect, Explore", paradigm),
                        format!("paradigm_strategies[{}].sequence[{}]", i, j),
                    ));
                }
            }
            // Sub-agents: name and system_prompt must be non-empty
            for (j, sub_agent) in strategy.sub_agents.iter().enumerate() {
                if sub_agent.name.is_empty() {
                    issues.push(ValidationIssue::error_at(
                        "strategies",
                        "Sub-agent name must not be empty",
                        format!("paradigm_strategies[{}].sub_agents[{}].name", i, j),
                    ));
                }
                if sub_agent.system_prompt.is_empty() {
                    issues.push(ValidationIssue::warning_at(
                        "strategies",
                        "Sub-agent has no system prompt — may produce inconsistent behavior",
                        format!("paradigm_strategies[{}].sub_agents[{}].system_prompt", i, j),
                    ));
                }
                if sub_agent.available_tools.is_empty() {
                    issues.push(ValidationIssue::warning_at(
                        "strategies",
                        format!("Sub-agent '{}' has no available tools", sub_agent.name),
                        format!("paradigm_strategies[{}].sub_agents[{}].available_tools", i, j),
                    ));
                }
            }
        }

        // Compression template: preserve_fields should not be empty if specified
        if !config.compression_template.name.is_empty() && config.compression_template.preserve_fields.is_empty() {
            issues.push(ValidationIssue::warning("compression", "Compression template has name but no preserve_fields — no fields will be prioritized during compression"));
        }
    }

    /// Validate semantic aspects (cross-layer consistency, reference integrity).
    fn validate_semantic(config: &DomainPackConfig, issues: &mut Vec<ValidationIssue>) {
        let known_tools = DomainPackSpec::known_tool_names();
        let known_tools_set: HashSet<&str> = known_tools.iter().map(|s| s.as_str()).collect();
        let known_sources = DomainPackSpec::known_context_source_names();
        let known_sources_set: HashSet<&str> = known_sources.iter().map(|s| s.as_str()).collect();
        let declared_tools: HashSet<&str> = config.tools.iter().map(|s| s.as_str()).collect();

        // Check: tool names reference known tools
        for (i, tool_name) in config.tools.iter().enumerate() {
            if !known_tools_set.contains(tool_name.as_str()) {
                issues.push(ValidationIssue::warning_at(
                    "tools",
                    format!("Tool '{}' is not in the known tool registry — it will be skipped during resolution", tool_name),
                    format!("tools[{}]", i),
                ));
            }
        }

        // Check: context source names reference known sources
        for (i, source_name) in config.context_sources.iter().enumerate() {
            if !known_sources_set.contains(source_name.as_str()) {
                issues.push(ValidationIssue::warning_at(
                    "context",
                    format!("Context source '{}' is not in the known source registry — it will be skipped during resolution", source_name),
                    format!("context_sources[{}]", i),
                ));
            }
        }

        // Check: permission profile tool references exist in tools list
        for (i, tool_name) in config.permission_profile.auto_approve.iter().enumerate() {
            if !declared_tools.contains(tool_name.as_str()) {
                issues.push(ValidationIssue::warning_at(
                    "permissions",
                    format!("Auto-approve tool '{}' is not declared in tools list — permission rule will have no effect", tool_name),
                    format!("permission_profile.auto_approve[{}]", i),
                ));
            }
        }

        for (i, tool_name) in config.permission_profile.require_confirmation.iter().enumerate() {
            if !declared_tools.contains(tool_name.as_str()) {
                issues.push(ValidationIssue::warning_at(
                    "permissions",
                    format!("Require-confirmation tool '{}' is not declared in tools list — permission rule will have no effect", tool_name),
                    format!("permission_profile.require_confirmation[{}]", i),
                ));
            }
        }

        for (i, deny) in config.permission_profile.deny_by_default.iter().enumerate() {
            // Deny patterns can be regex, so we check only exact matches
            if !deny.tool.contains('*') && !deny.tool.contains('.') && !declared_tools.contains(deny.tool.as_str()) {
                issues.push(ValidationIssue::info_at(
                    "permissions",
                    format!("Deny pattern tool '{}' is not in declared tools — pattern may match future tools", deny.tool),
                    format!("permission_profile.deny_by_default[{}].tool", i),
                ));
            }
        }

        // Check: paradigm strategy trigger patterns are valid regex
        for (i, strategy) in config.paradigm_strategies.iter().enumerate() {
            if let Err(e) = regex::Regex::new(&strategy.trigger) {
                issues.push(ValidationIssue::error_at(
                    "strategies",
                    format!("Invalid trigger pattern regex '{}': {}", strategy.trigger, e),
                    format!("paradigm_strategies[{}].trigger", i),
                ));
            }
        }

        // Check: sub-agent available_tools are subsets of declared tools
        for (i, strategy) in config.paradigm_strategies.iter().enumerate() {
            for (j, sub_agent) in strategy.sub_agents.iter().enumerate() {
                for tool_name in &sub_agent.available_tools {
                    if !declared_tools.contains(tool_name.as_str()) && !known_tools_set.contains(tool_name.as_str()) {
                        issues.push(ValidationIssue::warning_at(
                            "strategies",
                            format!("Sub-agent '{}' available tool '{}' is not in declared tools — it will be skipped", sub_agent.name, tool_name),
                            format!("paradigm_strategies[{}].sub_agents[{}].available_tools", i, j),
                        ));
                    }
                }
            }
        }

        // Check: system_prompt is non-empty when paradigm strategies are defined
        if !config.paradigm_strategies.is_empty() && config.system_prompt.is_empty() {
            issues.push(ValidationIssue::warning(
                "general",
                "System prompt is empty but paradigm strategies are defined — agents need instructions to decide which strategy to apply",
            ));
        }

        // Check: duplicate paradigm strategy triggers
        let mut triggers = HashSet::new();
        for strategy in &config.paradigm_strategies {
            if triggers.contains(&strategy.trigger) {
                issues.push(ValidationIssue::warning_at(
                    "strategies",
                    format!("Duplicate trigger pattern '{}' — first match will win", strategy.trigger),
                    "paradigm_strategies",
                ));
            }
            triggers.insert(strategy.trigger.clone());
        }

        // Check: tool_decorators reference existing tools
        for (tool_name, _description) in &config.tool_decorators {
            if !declared_tools.contains(tool_name.as_str()) && !known_tools_set.contains(tool_name.as_str()) {
                issues.push(ValidationIssue::warning_at(
                    "tools",
                    format!("Tool decorator for '{}' references a tool not in the tools list — decorator will have no effect", tool_name),
                    format!("tool_decorators['{}']", tool_name),
                ));
            }
        }
    }
}

// ─── Helper for warning_at ────────────────────────────────────────────────────

impl ValidationIssue {
    /// Create a Warning-level issue with a specific location.
    fn warning_at(layer: impl Into<String>, message: impl Into<String>, location: impl Into<String>) -> Self {
        Self {
            severity: ValidationSeverity::Warning,
            layer: layer.into(),
            message: message.into(),
            location: Some(location.into()),
        }
    }

    /// Create an Info-level issue with a specific location.
    fn info_at(layer: impl Into<String>, message: impl Into<String>, location: impl Into<String>) -> Self {
        Self {
            severity: ValidationSeverity::Info,
            layer: layer.into(),
            message: message.into(),
            location: Some(location.into()),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_parser::*;
    use std::collections::HashMap;

    fn make_valid_config() -> DomainPackConfig {
        DomainPackConfig {
            name: "coding".to_string(),
            description: "A coding agent".to_string(),
            tools: vec!["read_file".to_string(), "calculator".to_string()],
            tool_decorators: HashMap::new(),
            context_sources: vec!["date".to_string()],
            permission_profile: PermissionProfileConfig {
                auto_approve: vec!["read_file".to_string(), "calculator".to_string()],
                require_confirmation: vec![],
                deny_by_default: vec![],
            },
            paradigm_strategies: vec![ParadigmStrategyConfig {
                trigger: "implement|refactor".to_string(),
                sequence: vec!["Plan".to_string(), "ReAct".to_string()],
                sub_agents: vec![SubAgentConfig {
                    name: "code".to_string(),
                    description: "Code sub-agent".to_string(),
                    system_prompt: "You are a code agent".to_string(),
                    available_tools: vec!["read_file".to_string()],
                    permission_threshold: "standard".to_string(),
                    modifies_files: false,
                }],
                description: "Implementation workflow".to_string(),
            }],
            compression_template: CompressionTemplateConfig {
                name: "coding".to_string(),
                preserve_fields: vec!["critical_files".to_string()],
                truncate_rules: HashMap::new(),
            },
            system_prompt: "You are a coding agent".to_string(),
        }
    }

    #[test]
    fn test_valid_config_passes() {
        let config = make_valid_config();
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid());
        assert!(result.errors().is_empty());
    }

    #[test]
    fn test_empty_name_is_error() {
        let mut config = make_valid_config();
        config.name = String::new();
        let result = DomainPackValidator::validate(&config);
        assert!(!result.is_valid());
        assert!(result.errors().iter().any(|e| e.message.contains("name must not be empty")));
    }

    #[test]
    fn test_name_starts_with_digit_is_error() {
        let mut config = make_valid_config();
        config.name = "123coding".to_string();
        let result = DomainPackValidator::validate(&config);
        assert!(!result.is_valid());
        assert!(result.errors().iter().any(|e| e.message.contains("lowercase letter")));
    }

    #[test]
    fn test_name_with_invalid_char_is_error() {
        let mut config = make_valid_config();
        config.name = "My Coding Pack".to_string(); // uppercase + spaces
        let result = DomainPackValidator::validate(&config);
        assert!(!result.is_valid());
        assert!(result.errors().iter().any(|e| e.message.contains("invalid character")));
    }

    #[test]
    fn test_empty_tools_is_warning() {
        let mut config = make_valid_config();
        config.tools = Vec::new();
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid()); // Warning doesn't block
        assert!(result.warnings().iter().any(|w| w.message.contains("no tools")));
    }

    #[test]
    fn test_unknown_tool_is_warning() {
        let mut config = make_valid_config();
        config.tools.push("nonexistent_tool".to_string());
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid()); // Warning doesn't block
        assert!(result.warnings().iter().any(|w| w.message.contains("not in the known tool registry")));
    }

    #[test]
    fn test_duplicate_tools_is_error() {
        let mut config = make_valid_config();
        config.tools.push("read_file".to_string()); // duplicate
        let result = DomainPackValidator::validate(&config);
        assert!(!result.is_valid());
        assert!(result.errors().iter().any(|e| e.message.contains("Duplicate tool")));
    }

    #[test]
    fn test_permission_references_unknown_tool_is_warning() {
        let mut config = make_valid_config();
        config.permission_profile.auto_approve.push("unknown_tool".to_string());
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid()); // Warning doesn't block
        assert!(result.warnings().iter().any(|w| w.message.contains("not declared in tools")));
    }

    #[test]
    fn test_invalid_trigger_regex_is_error() {
        let mut config = make_valid_config();
        config.paradigm_strategies[0].trigger = "[invalid".to_string(); // invalid regex
        let result = DomainPackValidator::validate(&config);
        assert!(!result.is_valid());
        assert!(result.errors().iter().any(|e| e.message.contains("Invalid trigger pattern")));
    }

    #[test]
    fn test_invalid_paradigm_name_is_error() {
        let mut config = make_valid_config();
        config.paradigm_strategies[0].sequence.push("UnknownParadigm".to_string());
        let result = DomainPackValidator::validate(&config);
        assert!(!result.is_valid());
        assert!(result.errors().iter().any(|e| e.message.contains("Invalid paradigm name")));
    }

    #[test]
    fn test_empty_system_prompt_with_strategies_is_warning() {
        let mut config = make_valid_config();
        config.system_prompt = String::new();
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid()); // Warning doesn't block
        assert!(result.warnings().iter().any(|w| w.message.contains("System prompt is empty")));
    }

    #[test]
    fn test_unknown_context_source_is_warning() {
        let mut config = make_valid_config();
        config.context_sources.push("unknown_source".to_string());
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid()); // Warning doesn't block
        assert!(result.warnings().iter().any(|w| w.message.contains("not in the known source")));
    }

    #[test]
    fn test_empty_compression_preserve_fields_is_warning() {
        let mut config = make_valid_config();
        config.compression_template.preserve_fields = Vec::new();
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid()); // Warning doesn't block
        assert!(result.warnings().iter().any(|w| w.message.contains("no preserve_fields")));
    }

    #[test]
    fn test_sub_agent_unknown_tool_is_warning() {
        let mut config = make_valid_config();
        config.paradigm_strategies[0].sub_agents[0].available_tools.push("unknown_tool".to_string());
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid()); // Warning doesn't block
        assert!(result.warnings().iter().any(|w| w.message.contains("not in declared tools")));
    }

    #[test]
    fn test_empty_permission_profile_is_warning() {
        let mut config = make_valid_config();
        config.permission_profile.auto_approve = Vec::new();
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid()); // Warning doesn't block
        assert!(result.warnings().iter().any(|w| w.message.contains("no rules")));
    }

    #[test]
    fn test_duplicate_trigger_pattern_is_warning() {
        let mut config = make_valid_config();
        config.paradigm_strategies.push(ParadigmStrategyConfig {
            trigger: "implement|refactor".to_string(), // same as first
            sequence: vec!["ReAct".to_string()],
            sub_agents: Vec::new(),
            description: "Duplicate".to_string(),
        });
        let result = DomainPackValidator::validate(&config);
        assert!(result.is_valid()); // Warning doesn't block
        assert!(result.warnings().iter().any(|w| w.message.contains("Duplicate trigger")));
    }

    #[test]
    fn test_validation_result_accessors() {
        let issues = vec![
            ValidationIssue::error("test", "an error"),
            ValidationIssue::warning("test", "a warning"),
            ValidationIssue::info("test", "an info"),
        ];
        let result = ValidationResult::from_issues(issues);
        assert!(!result.is_valid());
        assert_eq!(result.errors().len(), 1);
        assert_eq!(result.warnings().len(), 1);
        assert_eq!(result.infos().len(), 1);
        assert_eq!(result.issues().len(), 3);
    }

    #[test]
    fn test_validation_severity_display() {
        assert_eq!(format!("{}", ValidationSeverity::Error), "ERROR");
        assert_eq!(format!("{}", ValidationSeverity::Warning), "WARNING");
        assert_eq!(format!("{}", ValidationSeverity::Info), "INFO");
    }
}
