//! DomainPack configuration parser — YAML/TOML → DomainPack conversion.
//!
//! This addresses the "DomainPack YAML/TOML 格式" gap (M19). Currently,
//! DomainPack requires Rust code (coding_pack(), research_pack()). For real
//! deployment, domain config should be loaded from files — operators configure
//! behavior without touching code.
//!
//! This is core to OneAI's "infrastructure positioning" (notebook Insight #1):
//! OneAI = Agent capability infrastructure, not "better Claude Code."
//! Infrastructure should be configurable from files, not hardcoded in code.
//!
//! **Usage**:
//! ```ignore
//! // Load domain from YAML config file:
//! let pack = domain_pack_from_file("ONEAI.domain.yaml", &tool_registry)?;
//!
//! // Or from TOML:
//! let pack = domain_pack_from_file("ONEAI.domain.toml", &tool_registry)?;
//!
//! // Search order in project directory:
//! let pack = domain_pack_from_dir("/project", &tool_registry)?;
//! // Checks: ONEAI.domain.yaml → ONEAI.domain.toml → fallback to coding_pack()
//! ```
//!
//! **Config format** (YAML example):
//! ```yaml
//! name: research
//! description: "Research domain pack — web-centric, read-only agent"
//! tools: [web_search, web_fetch, read_file, grep, glob, calculator]
//! context_sources: [project_instructions, date, environment]
//! permission_profile:
//!   auto_approve: [web_search, web_fetch, read_file, grep, calculator]
//!   require_confirmation: []
//!   deny_by_default:
//!     - tool: shell
//!       args_pattern: ".*"
//!       reason: "Shell not available in research mode"
//! paradigm_strategies:
//!   - trigger: "research|investigate|analyze"
//!     sequence: [Explore, Reflect, Plan]
//!     sub_agents:
//!       - name: searcher
//!         description: "Searches the web for information"
//!         system_prompt: "You are a search agent..."
//!         available_tools: [web_search, web_fetch, read_file]
//!         permission_threshold: standard
//! compression_template:
//!   name: research
//!   preserve_fields: [search_queries, key_findings, source_citations, conclusions]
//!   truncate_rules:
//!     search_result: 500
//!     web_content: 3000
//! system_prompt: "You are a research agent..."
//! ```

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use oneai_core::PermissionLevel;
use oneai_core::traits::Tool;
use oneai_tool::{
    FileReadTool, FileEditTool, GrepTool, GlobTool, FileListTool,
    ShellTool, WebSearchTool, WebFetchTool, EnvironmentTool,
    CalculatorTool, NotebookEditTool, ApplyPatchTool,
};

use crate::domain_pack::DomainPack;
use crate::tool_decorator::ToolDecorator;
use crate::permission_profile::{PermissionProfile, DenyPattern};
use crate::paradigm_strategy::{ParadigmStrategy, SubAgentTypeDefinition, DomainParadigmKind};
use crate::compression_template::CompressionTemplate;
use crate::builtin_sources::{
    GitStatusSource, FileTreeSource, ProjectConfigSource,
    DateSource, EnvironmentInfoSource, ProjectInstructionsSource,
};
use crate::context_source::ContextSource;

// ─── DomainPackConfig (serde-deserializable) ────────────────────────────────────

/// The YAML/TOML representation of a DomainPack.
///
/// This struct mirrors DomainPack's fields but uses string references
/// instead of Arc<dyn Trait> objects. After deserialization, the string
/// references are resolved to actual objects via `resolve()`.
///
/// All string-based references (tool names, context source names) are
/// resolved through predefined lookup tables:
/// - Tool names → Arc<dyn Tool> instances
/// - Context source names → Arc<dyn ContextSource> instances
/// - Permission thresholds → PermissionLevel enum values
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DomainPackConfig {
    /// Unique domain name (e.g., "coding", "research", "data_analysis").
    pub name: String,

    /// Human-readable description.
    #[serde(default)]
    pub description: String,

    /// Tool names to include (resolved from predefined tool factories).
    pub tools: Vec<String>,

    /// Tool decorator overrides — tool name → custom description.
    #[serde(default)]
    pub tool_decorators: HashMap<String, String>,

    /// Context source names to include (resolved from predefined factories).
    #[serde(default)]
    pub context_sources: Vec<String>,

    /// Permission profile configuration.
    pub permission_profile: PermissionProfileConfig,

    /// Paradigm strategy definitions.
    #[serde(default)]
    pub paradigm_strategies: Vec<ParadigmStrategyConfig>,

    /// Compression template configuration.
    #[serde(default)]
    pub compression_template: CompressionTemplateConfig,

    /// System prompt template.
    #[serde(default)]
    pub system_prompt: String,
}

/// Permission profile in config format (all string-based).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionProfileConfig {
    /// Tool names to auto-approve (skip approval gate).
    #[serde(default)]
    pub auto_approve: Vec<String>,

    /// Tool names that require explicit confirmation.
    #[serde(default)]
    pub require_confirmation: Vec<String>,

    /// Deny patterns — always block matching tool calls.
    #[serde(default)]
    pub deny_by_default: Vec<DenyPatternConfig>,
}

/// Deny pattern in config format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DenyPatternConfig {
    /// Tool name pattern (exact or regex).
    pub tool: String,

    /// Optional regex pattern matching tool arguments.
    #[serde(default)]
    pub args_pattern: Option<String>,

    /// Reason for denial (shown to user and model).
    pub reason: String,
}

/// Paradigm strategy in config format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParadigmStrategyConfig {
    /// Regex pattern for matching task descriptions.
    pub trigger: String,

    /// Paradigm sequence (Plan, ReAct, Reflect, Explore).
    pub sequence: Vec<String>,

    /// Sub-agent type definitions.
    #[serde(default)]
    pub sub_agents: Vec<SubAgentConfig>,

    /// Strategy description.
    #[serde(default)]
    pub description: String,
}

/// Sub-agent type in config format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubAgentConfig {
    /// Unique name for this sub-agent type.
    pub name: String,

    /// Human-readable description.
    pub description: String,

    /// System prompt for this sub-agent.
    pub system_prompt: String,

    /// Tool names available to this sub-agent.
    pub available_tools: Vec<String>,

    /// Permission threshold ("read", "standard", "admin").
    #[serde(default = "default_permission_threshold")]
    pub permission_threshold: String,

    /// Whether this sub-agent modifies files (needs worktree isolation).
    #[serde(default)]
    pub modifies_files: bool,
}

fn default_permission_threshold() -> String {
    "standard".to_string()
}

/// Compression template in config format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CompressionTemplateConfig {
    /// Template name.
    pub name: String,

    /// Fields to preserve during compression.
    #[serde(default)]
    pub preserve_fields: Vec<String>,

    /// Truncation rules: content_type → max chars.
    #[serde(default)]
    pub truncate_rules: HashMap<String, usize>,
}

// ─── Resolution: Config → DomainPack ───────────────────────────────────────────

/// Resolve a DomainPackConfig into an actual DomainPack.
///
/// This converts string-based references into Arc<dyn Tool> and
/// Arc<dyn ContextSource> instances using predefined lookup tables.
///
/// Unknown tool names or context source names are silently skipped
/// (with a warning log), allowing config files to reference tools
/// that aren't available in the current environment.
pub fn resolve_config(config: &DomainPackConfig, project_dir: &str) -> DomainPack {
    // Resolve tools from string names
    let tools = resolve_tools(&config.tools);

    // Resolve tool decorators
    let tool_decorators = config.tool_decorators.iter().map(|(name, desc)| {
        ToolDecorator::with_description(name, desc)
    }).collect();

    // Resolve context sources from string names
    let context_sources = resolve_context_sources(&config.context_sources, project_dir);

    // Resolve permission profile
    let permission_profile = resolve_permission_profile(&config.permission_profile);

    // Resolve paradigm strategies
    let paradigm_strategies = config.paradigm_strategies.iter().map(|s| {
        resolve_paradigm_strategy(s)
    }).collect();

    // Resolve compression template
    let compression_template = resolve_compression_template(&config.compression_template);

    DomainPack {
        name: config.name.clone(),
        description: config.description.clone(),
        tools,
        tool_decorators,
        context_sources,
        permission_profile,
        paradigm_strategies,
        compression_template,
        memory_profile: crate::memory_profile::MemoryProfile::default(),
        system_prompt_template: config.system_prompt.clone(),
        workflows: Vec::new(),
        state_graphs: Vec::new(),
        sub_agent_definitions: Vec::new(),
    }
}
// ─── Tool Resolution ──────────────────────────────────────────────────────────

/// Predefined tool factories — map tool name strings to Arc<dyn Tool> instances.
///
/// Each name maps to a constructor function that creates the tool.
/// This allows config files to reference tools by name without knowing
/// the Rust type.
fn tool_factories() -> HashMap<String, fn() -> Arc<dyn Tool>> {
    let mut map: HashMap<String, fn() -> Arc<dyn Tool>> = HashMap::new();

    // Standard tools (available in most domains)
    map.insert("read_file".to_string(), || Arc::new(FileReadTool::new()) as Arc<dyn Tool>);
    map.insert("edit_file".to_string(), || Arc::new(FileEditTool::new()) as Arc<dyn Tool>);
    map.insert("grep".to_string(), || Arc::new(GrepTool::new()) as Arc<dyn Tool>);
    map.insert("glob".to_string(), || Arc::new(GlobTool::new()) as Arc<dyn Tool>);
    map.insert("list_directory".to_string(), || Arc::new(FileListTool::new()) as Arc<dyn Tool>);
    map.insert("shell".to_string(), || Arc::new(ShellTool::new()) as Arc<dyn Tool>);
    map.insert("environment".to_string(), || Arc::new(EnvironmentTool::new()) as Arc<dyn Tool>);
    map.insert("calculator".to_string(), || Arc::new(CalculatorTool::new()) as Arc<dyn Tool>);
    map.insert("notebook_edit".to_string(), || Arc::new(NotebookEditTool::new()) as Arc<dyn Tool>);
    map.insert("apply_patch".to_string(), || Arc::new(ApplyPatchTool::new()) as Arc<dyn Tool>);

    // Web tools (available in research/web-centric domains)
    map.insert("web_search".to_string(), || Arc::new(WebSearchTool::new()) as Arc<dyn Tool>);
    map.insert("web_fetch".to_string(), || Arc::new(WebFetchTool::new()) as Arc<dyn Tool>);

    map
}

/// Resolve tool names to Arc<dyn Tool> instances.
///
/// Unknown names are skipped with a warning.
fn resolve_tools(names: &[String]) -> Vec<Arc<dyn Tool>> {
    let factories = tool_factories();
    let mut tools = Vec::new();

    for name in names {
        if let Some(factory) = factories.get(name) {
            tools.push(factory());
        } else {
            tracing::warn!("DomainPack config: unknown tool name '{}' — skipped", name);
        }
    }

    tools
}

// ─── Context Source Resolution ─────────────────────────────────────────────────

/// Resolve context source names to Arc<dyn ContextSource> instances.
///
/// Context sources may need the project_dir parameter for initialization.
fn resolve_context_sources(names: &[String], project_dir: &str) -> Vec<Arc<dyn ContextSource>> {
    let mut sources: Vec<Arc<dyn ContextSource>> = Vec::new();

    for name in names {
        match name.as_str() {
            "project_instructions" => sources.push(Arc::new(ProjectInstructionsSource::new(project_dir))),
            "git_status" => sources.push(Arc::new(GitStatusSource::new(project_dir))),
            "file_tree" => sources.push(Arc::new(FileTreeSource::new(project_dir))),
            "project_config" => sources.push(Arc::new(ProjectConfigSource::new(project_dir))),
            "date" => sources.push(Arc::new(DateSource::new())),
            "environment" => sources.push(Arc::new(EnvironmentInfoSource::new())),
            other => tracing::warn!("DomainPack config: unknown context source '{}' — skipped", other),
        }
    }

    sources
}

// ─── Permission Profile Resolution ─────────────────────────────────────────────

fn resolve_permission_profile(config: &PermissionProfileConfig) -> PermissionProfile {
    PermissionProfile {
        name: "config".to_string(),
        auto_approve: config.auto_approve.iter().cloned().collect::<HashSet<String>>(),
        require_confirmation: config.require_confirmation.iter().cloned().collect::<HashSet<String>>(),
        deny_by_default: config.deny_by_default.iter().map(|d| {
            DenyPattern {
                tool_pattern: d.tool.clone(),
                arg_pattern: d.args_pattern.clone(),
                reason: d.reason.clone(),
            }
        }).collect(),
        permission_overrides: HashMap::new(),
        default_threshold: PermissionLevel::Standard,
    }
}

// ─── Paradigm Strategy Resolution ──────────────────────────────────────────────

fn resolve_paradigm_strategy(config: &ParadigmStrategyConfig) -> ParadigmStrategy {
    ParadigmStrategy {
        trigger_pattern: config.trigger.clone(),
        paradigm_sequence: config.sequence.iter().map(|s| {
            DomainParadigmKind::from_str(s)
        }).collect(),
        sub_agent_types: config.sub_agents.iter().map(|sa| {
            SubAgentTypeDefinition {
                name: sa.name.clone(),
                description: sa.description.clone(),
                system_prompt: sa.system_prompt.clone(),
                available_tools: sa.available_tools.clone(),
                permission_threshold: match sa.permission_threshold.as_str() {
                    "read" => PermissionLevel::Read,
                    "admin" => PermissionLevel::Full,
                    _ => PermissionLevel::Standard,
                },
                budget: 0, // Default: uses SubAgentKind's default budget
                modifies_files: sa.modifies_files,
                merge_strategy: if sa.modifies_files {
                    crate::paradigm_strategy::SubAgentMergeStrategy::Merge
                } else {
                    crate::paradigm_strategy::SubAgentMergeStrategy::PreserveOnly
                },
                structured_output: None, // Not configurable via YAML yet
            }
        }).collect(),
        description: config.description.clone(),
    }
}

// ─── Compression Template Resolution ───────────────────────────────────────────

fn resolve_compression_template(config: &CompressionTemplateConfig) -> CompressionTemplate {
    CompressionTemplate {
        name: config.name.clone(),
        preserve_fields: config.preserve_fields.clone(),
        template: String::new(), // Will use default if not specified
        truncate_rules: config.truncate_rules.clone(),
        default_variables: HashMap::new(),
    }
}

// ─── File Parsing ─────────────────────────────────────────────────────────────

/// Parse a DomainPackConfig from a YAML file.
pub fn parse_yaml(path: &Path) -> Result<DomainPackConfig, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let config: DomainPackConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

/// Parse a DomainPackConfig from a TOML file.
pub fn parse_toml(path: &Path) -> Result<DomainPackConfig, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let config: DomainPackConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Load a DomainPack from a config file (auto-detects format from extension).
///
/// Supports `.yaml`, `.yml`, and `.toml` extensions.
/// After parsing, resolves string references to actual objects.
pub fn domain_pack_from_file(path: &Path, project_dir: &str) -> Result<DomainPack, Box<dyn std::error::Error>> {
    let extension = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let config = match extension {
        "yaml" | "yml" => parse_yaml(path)?,
        "toml" => parse_toml(path)?,
        other => return Err(format!(
            "Unknown domain config file extension '{}' — expected .yaml, .yml, or .toml",
            other
        ).into()),
    };

    Ok(resolve_config(&config, project_dir))
}

/// Load a DomainPack from a project directory by searching for config files.
///
/// Search order (first found wins):
/// 1. `ONEAI.domain.yaml` in project root
/// 2. `ONEAI.domain.toml` in project root
/// 3. Fallback: coding_pack(project_dir) if no config file found
///
/// This mirrors the project instruction search pattern (ONEAI.md/CLAUDE.md/AGENTS.md).
pub fn domain_pack_from_dir(project_dir: &str) -> Result<DomainPack, Box<dyn std::error::Error>> {
    let dir = Path::new(project_dir);

    // Try YAML first
    let yaml_path = dir.join("ONEAI.domain.yaml");
    if yaml_path.exists() {
        tracing::info!("Loading domain config from {}", yaml_path.display());
        return domain_pack_from_file(&yaml_path, project_dir);
    }

    // Try YML variant
    let yml_path = dir.join("ONEAI.domain.yml");
    if yml_path.exists() {
        tracing::info!("Loading domain config from {}", yml_path.display());
        return domain_pack_from_file(&yml_path, project_dir);
    }

    // Try TOML
    let toml_path = dir.join("ONEAI.domain.toml");
    if toml_path.exists() {
        tracing::info!("Loading domain config from {}", toml_path.display());
        return domain_pack_from_file(&toml_path, project_dir);
    }

    // No config file found → fallback to coding_pack
    tracing::info!("No domain config file found in {}, using default coding pack", project_dir);
    Ok(crate::coding_pack(project_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_yaml_config() {
        let yaml = r#"
name: research
description: "Research domain pack"
tools: [web_search, web_fetch, read_file, grep, glob, calculator]
context_sources: [project_instructions, date, environment]
permission_profile:
  auto_approve: [web_search, web_fetch, read_file, grep, calculator]
  require_confirmation: []
  deny_by_default:
    - tool: shell
      args_pattern: ".*"
      reason: "Shell not available"
paradigm_strategies:
  - trigger: "research|investigate"
    sequence: [Explore, Reflect, Plan]
    sub_agents:
      - name: searcher
        description: "Searches the web"
        system_prompt: "You are a search agent"
        available_tools: [web_search, web_fetch]
    description: "Deep research"
compression_template:
  name: research
  preserve_fields: [search_queries, key_findings]
  truncate_rules:
    search_result: 500
system_prompt: "You are a research agent"
"#;

        let config: DomainPackConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "research");
        assert_eq!(config.tools.len(), 6);
        assert_eq!(config.context_sources.len(), 3);
        assert!(config.permission_profile.auto_approve.contains(&"web_search".to_string()));
        assert_eq!(config.paradigm_strategies.len(), 1);
        assert_eq!(config.paradigm_strategies[0].sequence.len(), 3);
    }

    #[test]
    fn test_parse_toml_config() {
        let toml = r#"
name = "coding"
description = "Coding domain pack"
tools = ["read_file", "edit_file", "shell", "grep", "glob"]
context_sources = ["project_instructions", "git_status", "file_tree"]
[permission_profile]
auto_approve = ["read_file", "grep", "glob"]
require_confirmation = ["edit_file", "shell"]
[[permission_profile.deny_by_default]]
tool = "shell"
args_pattern = "rm.*-rf"
reason = "Dangerous deletion"

[[paradigm_strategies]]
trigger = "implement|refactor"
sequence = ["Plan", "ReAct", "Reflect"]
description = "Implementation workflow"

[compression_template]
name = "coding"
preserve_fields = ["critical_files", "progress_status"]
"#;

        let config: DomainPackConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.name, "coding");
        assert_eq!(config.tools.len(), 5);
        assert_eq!(config.context_sources.len(), 3);
        assert!(config.permission_profile.auto_approve.contains(&"read_file".to_string()));
        assert_eq!(config.paradigm_strategies.len(), 1);
    }

    #[test]
    fn test_resolve_config_to_domain_pack() {
        let config = DomainPackConfig {
            name: "test_domain".to_string(),
            description: "Test domain".to_string(),
            tools: vec!["read_file".to_string(), "calculator".to_string(), "unknown_tool".to_string()],
            tool_decorators: HashMap::from([
                ("read_file".to_string(), "Read files for test purposes".to_string()),
            ]),
            context_sources: vec!["date".to_string(), "environment".to_string(), "unknown_source".to_string()],
            permission_profile: PermissionProfileConfig {
                auto_approve: vec!["read_file".to_string(), "calculator".to_string()],
                require_confirmation: vec![],
                deny_by_default: vec![],
            },
            paradigm_strategies: vec![],
            compression_template: CompressionTemplateConfig {
                name: "test".to_string(),
                preserve_fields: vec!["key_data".to_string()],
                truncate_rules: HashMap::new(),
            },
            system_prompt: "You are a test agent".to_string(),
        };

        let pack = resolve_config(&config, "/tmp/test_project");

        assert_eq!(pack.name, "test_domain");
        // 2 known tools + 1 unknown (skipped)
        assert_eq!(pack.tools.len(), 2);
        // 2 known sources + 1 unknown (skipped)
        assert_eq!(pack.context_sources.len(), 2);
        assert_eq!(pack.tool_decorators.len(), 1);
        assert_eq!(pack.permission_profile.auto_approve.len(), 2);
        assert_eq!(pack.system_prompt_template, "You are a test agent");
    }

    #[test]
    fn test_resolve_unknown_tools_are_skipped() {
        let config = DomainPackConfig {
            name: "minimal".to_string(),
            description: String::new(),
            tools: vec!["nonexistent_tool".to_string()],
            tool_decorators: HashMap::new(),
            context_sources: vec!["nonexistent_source".to_string()],
            permission_profile: PermissionProfileConfig {
                auto_approve: vec!["nonexistent_tool".to_string()],
                require_confirmation: vec![],
                deny_by_default: vec![],
            },
            paradigm_strategies: vec![],
            compression_template: CompressionTemplateConfig {
                name: "minimal".to_string(),
                preserve_fields: vec![],
                truncate_rules: HashMap::new(),
            },
            system_prompt: String::new(),
        };

        let pack = resolve_config(&config, "/tmp/test");
        assert_eq!(pack.tools.len(), 0); // Unknown tool skipped
        assert_eq!(pack.context_sources.len(), 0); // Unknown source skipped
        // Permission profile still contains the name even if tool doesn't exist
        assert!(pack.permission_profile.auto_approve.contains("nonexistent_tool"));
    }

    #[test]
    fn test_resolve_paradigm_strategy() {
        let strategy_config = ParadigmStrategyConfig {
            trigger: "research|investigate".to_string(),
            sequence: vec!["Explore".to_string(), "Reflect".to_string(), "Plan".to_string()],
            sub_agents: vec![SubAgentConfig {
                name: "searcher".to_string(),
                description: "Search agent".to_string(),
                system_prompt: "You search".to_string(),
                available_tools: vec!["web_search".to_string()],
                permission_threshold: "standard".to_string(),
                modifies_files: false,
            }],
            description: "Research workflow".to_string(),
        };

        let strategy = resolve_paradigm_strategy(&strategy_config);
        assert_eq!(strategy.trigger_pattern, "research|investigate");
        assert_eq!(strategy.paradigm_sequence.len(), 3);
        assert_eq!(strategy.paradigm_sequence[0], DomainParadigmKind::Explore);
        assert_eq!(strategy.sub_agent_types.len(), 1);
        assert_eq!(strategy.sub_agent_types[0].name, "searcher");
    }

    #[test]
    fn test_permission_threshold_resolution() {
        assert_eq!(match "read" { "read" => PermissionLevel::Read, _ => PermissionLevel::Standard }, PermissionLevel::Read);
        assert_eq!(match "admin" { "admin" => PermissionLevel::Full, _ => PermissionLevel::Standard }, PermissionLevel::Full);
        assert_eq!(match "standard" { "standard" => PermissionLevel::Standard, _ => PermissionLevel::Standard }, PermissionLevel::Standard);
    }

    #[test]
    fn test_file_extension_detection() {
        let yaml_path = Path::new("ONEAI.domain.yaml");
        assert_eq!(yaml_path.extension().unwrap(), "yaml");

        let toml_path = Path::new("ONEAI.domain.toml");
        assert_eq!(toml_path.extension().unwrap(), "toml");
    }
}
