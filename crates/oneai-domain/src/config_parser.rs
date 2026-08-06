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
use std::time::Duration;

use oneai_core::traits::Tool;
use oneai_core::{DecayPolicy, FactType, PermissionLevel, RecallConfig};
use oneai_tool::{
    ApplyPatchTool, CalculatorTool, EnvironmentTool, FileEditTool, FileListTool, FileReadTool,
    GlobTool, GrepTool, NotebookEditTool, ShellTool, WebFetchTool, WebSearchTool,
};
use serde::{Deserialize, Serialize};

use crate::builtin_sources::{
    DateSource, EnvironmentInfoSource, FileTreeSource, GitStatusSource, ProjectConfigSource,
    ProjectInstructionsSource,
};
use crate::compression_template::CompressionTemplate;
use crate::context_source::ContextSource;
use crate::domain_pack::DomainPack;
use crate::memory_profile::{MemoryProfile, SkillLifecyclePolicy, WorkingStatePolicy};
use crate::paradigm_strategy::{DomainParadigmKind, ParadigmStrategy, SubAgentTypeDefinition};
use crate::permission_profile::{DenyPattern, PermissionProfile};
use crate::tool_decorator::ToolDecorator;

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

    /// Memory profile configuration (layer 7) — what to extract as durable
    /// facts, how to recall them, decay/forgetting, working-state persistence,
    /// and skill lifecycle. Spec-ified in E0 so the self-evolution loop can
    /// mutate memory strategy through the same validate→build→hot-load path as
    /// the other layers.
    #[serde(default)]
    pub memory_profile: MemoryProfileConfig,
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

/// Memory profile in config format (layer 7).
///
/// Mirrors `MemoryProfile` but uses serde-friendly primitives: `Vec<String>`
/// for fact-type lists (built via `FactType::new` on resolve), and
/// `WorkingStatePolicyConfig` / `SkillLifecyclePolicyConfig` for the two
/// sub-policies that carry `Duration` fields (Duration isn't serde-able, so
/// they're mirrored with `u64` secs). `recall` and `decay` have no Duration
/// and are already serde in `oneai-core`, so they're embedded directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryProfileConfig {
    /// Profile name (e.g. "coding", "research").
    #[serde(default)]
    pub name: String,

    /// Fact categories this domain extracts (→ `FactType` on resolve).
    #[serde(default)]
    pub extraction_schema: Vec<String>,

    /// How facts are recalled each turn (embedded — already serde in core).
    #[serde(default)]
    pub recall: RecallConfig,

    /// Token budget for the always-in-context core memory tier.
    #[serde(default = "default_core_budget_tokens")]
    pub core_budget_tokens: usize,

    /// Whether to expose self-managed memory tools to the agent.
    #[serde(default = "default_enable_memory_tools")]
    pub enable_memory_tools: bool,

    /// Fact types persisted under the user namespace (cross-session habits).
    #[serde(default)]
    pub habit_fact_types: Vec<String>,

    /// Memory decay / forgetting policy (embedded — already serde in core).
    #[serde(default)]
    pub decay: DecayPolicy,

    /// Working-state persistence + reconciliation policy.
    #[serde(default)]
    pub working_state: WorkingStatePolicyConfig,

    /// Skill lifecycle policy (retirement + backups).
    #[serde(default)]
    pub skill_lifecycle: SkillLifecyclePolicyConfig,
}

fn default_core_budget_tokens() -> usize {
    2048
}

fn default_enable_memory_tools() -> bool {
    true
}

/// Working-state policy in config format — `Duration` fields become `u64` secs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkingStatePolicyConfig {
    #[serde(default = "default_storage_root")]
    pub storage_root: String,
    #[serde(default = "default_checkpoint_granularity")]
    pub checkpoint_granularity: String,
    #[serde(default = "default_ground_truth_reconciliation")]
    pub ground_truth_reconciliation: String,
    #[serde(default = "default_cross_session_surface")]
    pub cross_session_surface: String,
    #[serde(default = "default_retention")]
    pub retention: String,
    #[serde(default = "default_thickness")]
    pub thickness: String,
    #[serde(default = "default_compaction_event_threshold")]
    pub compaction_event_threshold: usize,
    #[serde(default = "default_compaction_keep_recent")]
    pub compaction_keep_recent: usize,
    #[serde(default = "default_max_age_before_archive_secs")]
    pub max_age_before_archive_secs: u64,
}

/// Skill lifecycle policy in config format — `Duration` fields become `u64` secs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillLifecyclePolicyConfig {
    #[serde(default = "default_stale_after_secs")]
    pub stale_after_secs: u64,
    #[serde(default = "default_archive_after_secs")]
    pub archive_after_secs: u64,
    #[serde(default = "default_auto_transitions")]
    pub auto_transitions: bool,
    #[serde(default = "default_backup_count")]
    pub backup_count: usize,
    #[serde(default = "default_skill_storage_root")]
    pub storage_root: String,
    #[serde(default = "default_grace_unused_secs")]
    pub grace_unused_secs: u64,
}

// ── default string/numeric constants for the WorkingState/SkillLifecycle configs
fn default_storage_root() -> String {
    "in_repo".to_string()
}
fn default_checkpoint_granularity() -> String {
    "every_step".to_string()
}
fn default_ground_truth_reconciliation() -> String {
    "git".to_string()
}
fn default_cross_session_surface() -> String {
    "auto_inject".to_string()
}
fn default_retention() -> String {
    "archive_on_complete".to_string()
}
fn default_thickness() -> String {
    "thin".to_string()
}
fn default_compaction_event_threshold() -> usize {
    200
}
fn default_compaction_keep_recent() -> usize {
    50
}
fn default_max_age_before_archive_secs() -> u64 {
    30 * 24 * 3600
}
fn default_stale_after_secs() -> u64 {
    30 * 24 * 3600
}
fn default_archive_after_secs() -> u64 {
    90 * 24 * 3600
}
fn default_auto_transitions() -> bool {
    true
}
fn default_backup_count() -> usize {
    5
}
fn default_skill_storage_root() -> String {
    "home_dir".to_string()
}
fn default_grace_unused_secs() -> u64 {
    7 * 24 * 3600
}

// Manual Default impls — must agree with the serde `default = "..."` fns above
// (derive(Default) would zero-fill numeric fields, violating the ordering
// constraints checked by the validator: keep_recent<event_threshold,
// stale_after<archive_after).
impl Default for WorkingStatePolicyConfig {
    fn default() -> Self {
        Self {
            storage_root: default_storage_root(),
            checkpoint_granularity: default_checkpoint_granularity(),
            ground_truth_reconciliation: default_ground_truth_reconciliation(),
            cross_session_surface: default_cross_session_surface(),
            retention: default_retention(),
            thickness: default_thickness(),
            compaction_event_threshold: default_compaction_event_threshold(),
            compaction_keep_recent: default_compaction_keep_recent(),
            max_age_before_archive_secs: default_max_age_before_archive_secs(),
        }
    }
}

impl Default for SkillLifecyclePolicyConfig {
    fn default() -> Self {
        Self {
            stale_after_secs: default_stale_after_secs(),
            archive_after_secs: default_archive_after_secs(),
            auto_transitions: default_auto_transitions(),
            backup_count: default_backup_count(),
            storage_root: default_skill_storage_root(),
            grace_unused_secs: default_grace_unused_secs(),
        }
    }
}

impl Default for MemoryProfileConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            extraction_schema: Vec::new(),
            recall: RecallConfig::default(),
            core_budget_tokens: default_core_budget_tokens(),
            enable_memory_tools: default_enable_memory_tools(),
            habit_fact_types: Vec::new(),
            decay: DecayPolicy::default(),
            working_state: WorkingStatePolicyConfig::default(),
            skill_lifecycle: SkillLifecyclePolicyConfig::default(),
        }
    }
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
    let tool_decorators = config
        .tool_decorators
        .iter()
        .map(|(name, desc)| ToolDecorator::with_description(name, desc))
        .collect();

    // Resolve context sources from string names
    let context_sources = resolve_context_sources(&config.context_sources, project_dir);

    // Resolve permission profile
    let permission_profile = resolve_permission_profile(&config.permission_profile);

    // Resolve paradigm strategies
    let paradigm_strategies = config
        .paradigm_strategies
        .iter()
        .map(resolve_paradigm_strategy)
        .collect();

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
        memory_profile: resolve_memory_profile(&config.memory_profile),
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
    map.insert("read_file".to_string(), || {
        Arc::new(FileReadTool::new()) as Arc<dyn Tool>
    });
    map.insert("edit_file".to_string(), || {
        Arc::new(FileEditTool::new()) as Arc<dyn Tool>
    });
    map.insert("grep".to_string(), || {
        Arc::new(GrepTool::new()) as Arc<dyn Tool>
    });
    map.insert("glob".to_string(), || {
        Arc::new(GlobTool::new()) as Arc<dyn Tool>
    });
    map.insert("list_directory".to_string(), || {
        Arc::new(FileListTool::new()) as Arc<dyn Tool>
    });
    map.insert("shell".to_string(), || {
        Arc::new(ShellTool::new()) as Arc<dyn Tool>
    });
    map.insert("environment".to_string(), || {
        Arc::new(EnvironmentTool::new()) as Arc<dyn Tool>
    });
    map.insert("calculator".to_string(), || {
        Arc::new(CalculatorTool::new()) as Arc<dyn Tool>
    });
    map.insert("notebook_edit".to_string(), || {
        Arc::new(NotebookEditTool::new()) as Arc<dyn Tool>
    });
    map.insert("apply_patch".to_string(), || {
        Arc::new(ApplyPatchTool::new()) as Arc<dyn Tool>
    });

    // Web tools (available in research/web-centric domains)
    map.insert("web_search".to_string(), || {
        Arc::new(WebSearchTool::new()) as Arc<dyn Tool>
    });
    map.insert("web_fetch".to_string(), || {
        Arc::new(WebFetchTool::new()) as Arc<dyn Tool>
    });

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
            "project_instructions" => {
                sources.push(Arc::new(ProjectInstructionsSource::new(project_dir)))
            }
            "git_status" => sources.push(Arc::new(GitStatusSource::new(project_dir))),
            "file_tree" => sources.push(Arc::new(FileTreeSource::new(project_dir))),
            "project_config" => sources.push(Arc::new(ProjectConfigSource::new(project_dir))),
            "date" => sources.push(Arc::new(DateSource::new())),
            "environment" => sources.push(Arc::new(EnvironmentInfoSource::new())),
            other => tracing::warn!(
                "DomainPack config: unknown context source '{}' — skipped",
                other
            ),
        }
    }

    sources
}

// ─── Permission Profile Resolution ─────────────────────────────────────────────

fn resolve_permission_profile(config: &PermissionProfileConfig) -> PermissionProfile {
    PermissionProfile {
        name: "config".to_string(),
        auto_approve: config
            .auto_approve
            .iter()
            .cloned()
            .collect::<HashSet<String>>(),
        require_confirmation: config
            .require_confirmation
            .iter()
            .cloned()
            .collect::<HashSet<String>>(),
        deny_by_default: config
            .deny_by_default
            .iter()
            .map(|d| DenyPattern {
                tool_pattern: d.tool.clone(),
                arg_pattern: d.args_pattern.clone(),
                reason: d.reason.clone(),
            })
            .collect(),
        permission_overrides: HashMap::new(),
        default_threshold: PermissionLevel::Standard,
    }
}

// ─── Paradigm Strategy Resolution ──────────────────────────────────────────────

fn resolve_paradigm_strategy(config: &ParadigmStrategyConfig) -> ParadigmStrategy {
    ParadigmStrategy {
        trigger_pattern: config.trigger.clone(),
        paradigm_sequence: config
            .sequence
            .iter()
            .map(|s| DomainParadigmKind::from_str(s))
            .collect(),
        sub_agent_types: config
            .sub_agents
            .iter()
            .map(|sa| {
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
            })
            .collect(),
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

// ─── Memory Profile Resolution ─────────────────────────────────────────────────

fn resolve_memory_profile(config: &MemoryProfileConfig) -> MemoryProfile {
    MemoryProfile::new(&config.name)
        .extraction_schema(config.extraction_schema.iter().map(FactType::new).collect())
        .recall(config.recall.clone())
        .core_budget_tokens(config.core_budget_tokens)
        .enable_memory_tools(config.enable_memory_tools)
        .habit_fact_types(config.habit_fact_types.iter().map(FactType::new).collect())
        .decay(config.decay.clone())
        .working_state(resolve_working_state(&config.working_state))
        .skill_lifecycle(resolve_skill_lifecycle(&config.skill_lifecycle))
}

fn resolve_working_state(c: &WorkingStatePolicyConfig) -> WorkingStatePolicy {
    use crate::memory_profile::{
        CheckpointGranularity, CrossSessionSurface, GroundTruthReconciliation, Retention,
        StorageRoot, WorkingStateThickness,
    };
    WorkingStatePolicy {
        storage_root: StorageRoot::from_str(&c.storage_root),
        checkpoint_granularity: CheckpointGranularity::from_str(&c.checkpoint_granularity),
        ground_truth_reconciliation: GroundTruthReconciliation::from_str(
            &c.ground_truth_reconciliation,
        ),
        cross_session_surface: CrossSessionSurface::from_str(&c.cross_session_surface),
        retention: Retention::from_str(&c.retention),
        thickness: WorkingStateThickness::from_str(&c.thickness),
        compaction: crate::memory_profile::CompactionConfig {
            event_threshold: c.compaction_event_threshold,
            keep_recent: c.compaction_keep_recent,
        },
        max_age_before_archive: Duration::from_secs(c.max_age_before_archive_secs),
    }
}

fn resolve_skill_lifecycle(c: &SkillLifecyclePolicyConfig) -> SkillLifecyclePolicy {
    use crate::memory_profile::StorageRoot;
    SkillLifecyclePolicy {
        stale_after: Duration::from_secs(c.stale_after_secs),
        archive_after: Duration::from_secs(c.archive_after_secs),
        auto_transitions: c.auto_transitions,
        backup_count: c.backup_count,
        storage_root: StorageRoot::from_str(&c.storage_root),
        grace_unused: Duration::from_secs(c.grace_unused_secs),
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
pub fn domain_pack_from_file(
    path: &Path,
    project_dir: &str,
) -> Result<DomainPack, Box<dyn std::error::Error>> {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let config = match extension {
        "yaml" | "yml" => parse_yaml(path)?,
        "toml" => parse_toml(path)?,
        other => {
            return Err(format!(
                "Unknown domain config file extension '{}' — expected .yaml, .yml, or .toml",
                other
            )
            .into())
        }
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
    tracing::info!(
        "No domain config file found in {}, using default coding pack",
        project_dir
    );
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
        assert!(config
            .permission_profile
            .auto_approve
            .contains(&"web_search".to_string()));
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
        assert!(config
            .permission_profile
            .auto_approve
            .contains(&"read_file".to_string()));
        assert_eq!(config.paradigm_strategies.len(), 1);
    }

    #[test]
    fn test_resolve_config_to_domain_pack() {
        let config = DomainPackConfig {
            name: "test_domain".to_string(),
            description: "Test domain".to_string(),
            tools: vec![
                "read_file".to_string(),
                "calculator".to_string(),
                "unknown_tool".to_string(),
            ],
            tool_decorators: HashMap::from([(
                "read_file".to_string(),
                "Read files for test purposes".to_string(),
            )]),
            context_sources: vec![
                "date".to_string(),
                "environment".to_string(),
                "unknown_source".to_string(),
            ],
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
            memory_profile: MemoryProfileConfig::default(),
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
            memory_profile: MemoryProfileConfig::default(),
        };

        let pack = resolve_config(&config, "/tmp/test");
        assert_eq!(pack.tools.len(), 0); // Unknown tool skipped
        assert_eq!(pack.context_sources.len(), 0); // Unknown source skipped
                                                   // Permission profile still contains the name even if tool doesn't exist
        assert!(pack
            .permission_profile
            .auto_approve
            .contains("nonexistent_tool"));
    }

    #[test]
    fn test_resolve_paradigm_strategy() {
        let strategy_config = ParadigmStrategyConfig {
            trigger: "research|investigate".to_string(),
            sequence: vec![
                "Explore".to_string(),
                "Reflect".to_string(),
                "Plan".to_string(),
            ],
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
        assert_eq!(
            match "read" {
                "read" => PermissionLevel::Read,
                _ => PermissionLevel::Standard,
            },
            PermissionLevel::Read
        );
        assert_eq!(
            match "admin" {
                "admin" => PermissionLevel::Full,
                _ => PermissionLevel::Standard,
            },
            PermissionLevel::Full
        );
        assert_eq!(
            match "standard" {
                "standard" => PermissionLevel::Standard,
                _ => PermissionLevel::Standard,
            },
            PermissionLevel::Standard
        );
    }

    #[test]
    fn test_file_extension_detection() {
        let yaml_path = Path::new("ONEAI.domain.yaml");
        assert_eq!(yaml_path.extension().unwrap(), "yaml");

        let toml_path = Path::new("ONEAI.domain.toml");
        assert_eq!(toml_path.extension().unwrap(), "toml");
    }

    #[test]
    fn test_resolve_memory_profile_round_trip() {
        use oneai_core::{FactType, RecallStrategy};
        use std::time::Duration;

        // Config equivalent to MemoryProfile::coding()
        let cfg = MemoryProfileConfig {
            name: "coding".to_string(),
            extraction_schema: vec![
                "user_tooling_pref".to_string(),
                "decision".to_string(),
                "open_task".to_string(),
                "critical_file".to_string(),
            ],
            recall: oneai_core::RecallConfig {
                strategy: RecallStrategy::Hybrid,
                top_k: 5,
                time_decay: true,
                ..Default::default()
            },
            core_budget_tokens: 2048,
            enable_memory_tools: true,
            habit_fact_types: vec!["user_tooling_pref".to_string()],
            decay: oneai_core::DecayPolicy::default(),
            working_state: WorkingStatePolicyConfig {
                storage_root: "in_repo".to_string(),
                checkpoint_granularity: "every_step".to_string(),
                ground_truth_reconciliation: "git".to_string(),
                cross_session_surface: "auto_inject".to_string(),
                retention: "archive_on_complete".to_string(),
                thickness: "thin".to_string(),
                compaction_event_threshold: 200,
                compaction_keep_recent: 50,
                max_age_before_archive_secs: 30 * 24 * 3600,
            },
            skill_lifecycle: SkillLifecyclePolicyConfig {
                stale_after_secs: 30 * 24 * 3600,
                archive_after_secs: 90 * 24 * 3600,
                auto_transitions: true,
                backup_count: 5,
                storage_root: "home_dir".to_string(),
                grace_unused_secs: 7 * 24 * 3600,
            },
        };

        let mp = resolve_memory_profile(&cfg);
        assert_eq!(mp.name, "coding");
        assert_eq!(mp.extraction_schema.len(), 4);
        assert!(mp.extraction_schema.contains(&FactType::new("decision")));
        assert_eq!(mp.recall.strategy, RecallStrategy::Hybrid);
        assert_eq!(mp.recall.top_k, 5);
        assert!(mp.recall.time_decay);
        assert_eq!(mp.core_budget_tokens, 2048);
        assert!(mp.enable_memory_tools);
        assert_eq!(
            mp.habit_fact_types,
            vec![FactType::new("user_tooling_pref")]
        );
        assert!(!mp.decay.enabled);
        assert_eq!(mp.working_state.compaction.event_threshold, 200);
        assert_eq!(mp.working_state.compaction.keep_recent, 50);
        assert_eq!(
            mp.working_state.storage_root,
            crate::memory_profile::StorageRoot::InRepo
        );
        assert_eq!(
            mp.skill_lifecycle.stale_after,
            Duration::from_secs(30 * 24 * 3600)
        );
    }

    #[test]
    fn test_memory_profile_yaml_round_trip() {
        use oneai_core::RecallStrategy;

        let yaml = r#"
name: coding
description: "Coding pack with memory profile"
tools: [read_file, calculator]
context_sources: [date]
permission_profile:
  auto_approve: [read_file, calculator]
  require_confirmation: []
  deny_by_default: []
compression_template:
  name: coding
  preserve_fields: [critical_files]
memory_profile:
  name: coding
  extraction_schema: [user_tooling_pref, decision]
  recall:
    strategy: hybrid
    top_k: 5
    time_decay: true
  core_budget_tokens: 2048
  enable_memory_tools: true
  habit_fact_types: [user_tooling_pref]
  working_state:
    storage_root: in_repo
    compaction_event_threshold: 200
    compaction_keep_recent: 50
  skill_lifecycle:
    stale_after_secs: 2592000
    archive_after_secs: 7776000
system_prompt: "You are a coding agent"
"#;

        let config: DomainPackConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.memory_profile.name, "coding");
        assert_eq!(config.memory_profile.extraction_schema.len(), 2);
        assert_eq!(
            config.memory_profile.recall.strategy,
            RecallStrategy::Hybrid
        );
        assert_eq!(config.memory_profile.recall.top_k, 5);
        assert_eq!(config.memory_profile.core_budget_tokens, 2048);
        assert_eq!(
            config
                .memory_profile
                .working_state
                .compaction_event_threshold,
            200
        );
        assert_eq!(
            config.memory_profile.skill_lifecycle.archive_after_secs,
            7_776_000
        );
    }
}
