//! ParadigmStrategy — domain-specific task-to-paradigm mapping.
//!
//! The ParadigmStrategy maps user task patterns to specific paradigm sequences
//! and sub-agent configurations. This is the "4th layer" of the DomainPack system.
//!
//! Different domains have different typical task patterns:
//! - Coding: refactor → Plan+ReAct+Reflect, search → Explore
//! - Research: deep research → Search+Extract+Synthesize+Verify
//! - Data analysis: analyze → Query+Transform+Visualize+Interpret
//!
//! ParadigmKind stays unchanged (Plan/ReAct/Reflect/Explore). Domain packs
//! configure *within* paradigms (system prompt, tool set), not new paradigm types.

use serde::{Deserialize, Serialize};
use oneai_core::PermissionLevel;
use oneai_core::StructuredOutputConfig;

// ─── ParadigmKind ──────────────────────────────────────────────────────────────

/// Re-export ParadigmKind from oneai-agent for use in paradigm strategies.
///
/// We define our own copy here to avoid a circular dependency (oneai-domain
/// doesn't depend on oneai-agent). The actual ParadigmKind in oneai-agent
/// has the same values. When oneai-agent reads a DomainPack, it converts
/// between the two representations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DomainParadigmKind {
    Plan,
    ReAct,
    Reflect,
    Explore,
}

impl DomainParadigmKind {
    /// Convert to the ParadigmKind used in oneai-agent.
    pub fn to_agent_paradigm(&self) -> String {
        match self {
            Self::Plan => "plan".to_string(),
            Self::ReAct => "react".to_string(),
            Self::Reflect => "reflect".to_string(),
            Self::Explore => "explore".to_string(),
        }
    }

    /// Parse from string.
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "plan" => Self::Plan,
            "react" => Self::ReAct,
            "reflect" => Self::Reflect,
            "explore" => Self::Explore,
            _ => Self::ReAct, // Default fallback
        }
    }
}

// ─── SubAgentTypeDefinition ────────────────────────────────────────────────────

/// Definition of a domain-specific sub-agent type.
///
/// This extends the fixed `SubAgentKind` enum with custom types that have
/// domain-specific metadata: system prompt, tool set, permission threshold,
/// budget, isolation policy, and optional structured output validation.
///
/// When the main agent delegates to a `SubAgentKind::Custom(name)` sub-agent,
/// the DefaultSubAgentFactory looks up the SubAgentTypeDefinition by name
/// and uses its metadata to configure the sub-agent's behavior.
///
/// Additionally, standard sub-agent kinds (plan, explore, code, review)
/// can be overridden via these definitions when a DomainPack is active —
/// enabling domain-specific roles (e.g., a "code_review" sub-agent in a
/// CodingPack has different prompts and tools than a generic "review" sub-agent).
///
/// Examples:
/// - Coding: "searcher" — explores codebase with read+grep+glob tools
/// - Coding: "coder" — implements changes with edit+shell tools
/// - Research: "searcher" — searches web with web_search+web_fetch tools
/// - Research: "verifier" — verifies claims with citation tools
///
/// **Design note**: The `merge_strategy` field uses a DomainPack-level
/// `SubAgentMergeStrategy` enum (not `WorktreeConfig::MergeStrategy` from
/// `oneai-agent`) to keep this struct as a pure configuration layer.
/// The `DefaultSubAgentFactory` maps these at execution time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubAgentTypeDefinition {
    /// Unique name for this sub-agent type.
    /// Maps to `SubAgentKind::from_str()` in oneai-agent.
    /// Standard names: "plan", "explore", "code", "review".
    /// Custom names: any user-defined string (e.g., "security_review").
    pub name: String,

    /// Human-readable description of what this sub-agent does.
    /// Used in delegation prompts so the LLM knows what each type can do.
    pub description: String,

    /// System prompt for this sub-agent type.
    /// Defines the sub-agent's role, capabilities, and constraints.
    /// For standard kinds (plan, explore, etc.), this overrides the
    /// SubAgentKind's default_system_prompt(). For custom kinds,
    /// this is the primary behavioral specification.
    pub system_prompt: String,

    /// Tools available to this sub-agent (subset of domain tools).
    /// The sub-agent can only use these tools, not the full domain tool set.
    /// This applies at both the definition level (what the LLM sees) and
    /// the execution level (what the agent can actually call).
    pub available_tools: Vec<String>,

    /// Permission level threshold for this sub-agent.
    /// Tools at or below this level are auto-approved within the sub-agent.
    /// Tools above this level still require the approval gate.
    pub permission_threshold: PermissionLevel,

    /// Default token budget allocation for this sub-agent.
    /// Maximum tokens the sub-agent can consume during execution.
    /// If 0, uses a default budget (typically 50,000 tokens).
    #[serde(default)]
    pub budget: u32,

    /// Whether this sub-agent modifies files.
    /// If true, the DefaultSubAgentFactory will configure worktree
    /// isolation (git worktree for Code/Custom agents).
    /// If false, the sub-agent operates in the project directory directly.
    #[serde(default)]
    pub modifies_files: bool,

    /// Merge strategy for worktree isolation.
    /// Only relevant when `modifies_files = true`.
    /// Determines how the sub-agent's changes are merged back
    /// to the main branch after completion.
    #[serde(default = "default_merge_strategy")]
    pub merge_strategy: SubAgentMergeStrategy,

    /// Structured output schema for validating sub-agent return values.
    /// If Some, the SubAgentSummary returned by the sub-agent is validated
    /// against this JSON Schema. Validation failures trigger ModelRetry.
    /// If None, no structured output validation is applied.
    #[serde(default)]
    pub structured_output: Option<StructuredOutputConfig>,
}

/// Default merge strategy (PreserveOnly for read-only agents).
fn default_merge_strategy() -> SubAgentMergeStrategy {
    SubAgentMergeStrategy::PreserveOnly
}

impl SubAgentTypeDefinition {
    /// Create a standard "plan" sub-agent definition.
    pub fn plan() -> Self {
        Self {
            name: "plan".to_string(),
            description: "Task decomposition agent — creates structured plans".to_string(),
            system_prompt: "You are a planning agent. Decompose the given task into ordered steps \
                with dependencies. Return a structured plan as a numbered list.".to_string(),
            available_tools: vec!["read_file".into(), "grep".into(), "glob".into()],
            permission_threshold: PermissionLevel::Read,
            budget: 30_000,
            modifies_files: false,
            merge_strategy: SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
        }
    }

    /// Create a standard "explore" sub-agent definition.
    pub fn explore() -> Self {
        Self {
            name: "explore".to_string(),
            description: "Exploration agent — searches and understands the codebase".to_string(),
            system_prompt: "You are an exploration agent. Search and understand the codebase \
                using available tools. Return a comprehensive summary of your findings.".to_string(),
            available_tools: vec![
                "read_file".into(), "grep".into(), "glob".into(),
                "list_directory".into(),
            ],
            permission_threshold: PermissionLevel::Read,
            budget: 40_000,
            modifies_files: false,
            merge_strategy: SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
        }
    }

    /// Create a standard "code" sub-agent definition.
    pub fn code() -> Self {
        Self {
            name: "code".to_string(),
            description: "Code implementation agent — writes and modifies code".to_string(),
            system_prompt: "You are a code implementation agent. Write and modify code based \
                on the given specification. Return a summary of all changes you made.".to_string(),
            available_tools: vec![
                "read_file".into(), "edit_file".into(), "shell".into(),
                "grep".into(), "glob".into(),
            ],
            permission_threshold: PermissionLevel::Standard,
            budget: 80_000,
            modifies_files: true,
            merge_strategy: SubAgentMergeStrategy::Merge,
            structured_output: None,
        }
    }

    /// Create a standard "review" sub-agent definition.
    pub fn review() -> Self {
        Self {
            name: "review".to_string(),
            description: "Review agent — reviews and audits existing work".to_string(),
            system_prompt: "You are a code review agent. Review code for correctness bugs, \
                style issues, and potential improvements. Return a structured review.".to_string(),
            available_tools: vec![
                "read_file".into(), "grep".into(), "glob".into(),
                "list_directory".into(),
            ],
            permission_threshold: PermissionLevel::Read,
            budget: 30_000,
            modifies_files: false,
            merge_strategy: SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
        }
    }

    /// Create a custom sub-agent definition.
    pub fn custom(name: &str, description: &str, system_prompt: &str, tools: Vec<String>, budget: u32, modifies_files: bool) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            system_prompt: system_prompt.to_string(),
            available_tools: tools,
            permission_threshold: if modifies_files { PermissionLevel::Standard } else { PermissionLevel::Read },
            budget,
            modifies_files,
            merge_strategy: if modifies_files {
                SubAgentMergeStrategy::Merge
            } else {
                SubAgentMergeStrategy::PreserveOnly
            },
            structured_output: None,
        }
    }

    /// Get all standard sub-agent definitions (plan, explore, code, review).
    pub fn defaults() -> Vec<Self> {
        vec![Self::plan(), Self::explore(), Self::code(), Self::review()]
    }
}

// ─── SubAgentMergeStrategy ──────────────────────────────────────────────────────

/// Merge strategy for worktree isolation — DomainPack-level configuration.
///
/// This is a simplified version of `WorktreeConfig::MergeStrategy` from
/// `oneai-agent`. The `DefaultSubAgentFactory` maps these to the
/// corresponding `worktree_isolation::MergeStrategy` at execution time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubAgentMergeStrategy {
    /// Merge the worktree branch into the main branch.
    Merge,

    /// Rebase the worktree branch onto the main branch, then fast-forward.
    Rebase,

    /// Cherry-pick specific commits from the worktree branch.
    CherryPick,

    /// Don't merge — preserve the worktree branch for manual review.
    PreserveOnly,
}

// ─── ParadigmStrategy ──────────────────────────────────────────────────────────

/// Maps a task pattern to a specific paradigm sequence and sub-agent configuration.
///
/// When the user's task matches the trigger_pattern (regex), this strategy
/// determines the sequence of paradigms to apply and the sub-agent types
/// available for delegation.
///
/// Example (CodingPack):
/// - trigger_pattern: "refactor|rewrite|restructure"
/// - paradigm_sequence: [Plan, ReAct, Reflect]
/// - sub_agent_types: [PlanSubAgent, ExploreSubAgent, CodeSubAgent]
///
/// This means when the user asks to "refactor the auth module", the agent:
/// 1. Plans the approach (Plan paradigm)
/// 2. Executes the plan (ReAct paradigm)
/// 3. Reflects on the results (Reflect paradigm)
/// And can delegate to specialized sub-agents defined in sub_agent_types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParadigmStrategy {
    /// Regex pattern that triggers this strategy when matched against the user's task.
    ///
    /// The first matching strategy wins (strategies are checked in order).
    /// Examples: "refactor|rewrite|restructure", "find|search|understand|explain"
    pub trigger_pattern: String,

    /// Ordered paradigm sequence to apply when this strategy is triggered.
    ///
    /// The agent applies paradigms in this order. Each paradigm changes the
    /// agent's reasoning style and available tools.
    pub paradigm_sequence: Vec<DomainParadigmKind>,

    /// Sub-agent types available in this strategy context.
    ///
    /// These types are registered in the SubAgentFactory when this
    /// domain pack is active, enabling the main agent to delegate
    /// to specialized sub-agents.
    pub sub_agent_types: Vec<SubAgentTypeDefinition>,

    /// Human-readable description of when this strategy applies.
    pub description: String,
}

impl ParadigmStrategy {
    /// Check if a task description matches this strategy's trigger pattern.
    pub fn matches(&self, task: &str) -> bool {
        regex::RegexBuilder::new(&self.trigger_pattern)
            .case_insensitive(true)
            .build()
            .map(|re| re.is_match(task))
            .unwrap_or(false)
    }
}

// ─── ParadigmStrategyRegistry ──────────────────────────────────────────────────

/// Registry of paradigm strategies, used for matching tasks to strategies.
pub struct ParadigmStrategyRegistry {
    strategies: Vec<ParadigmStrategy>,
}

impl ParadigmStrategyRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self { strategies: Vec::new() }
    }

    /// Create from a list of strategies.
    pub fn from_strategies(strategies: Vec<ParadigmStrategy>) -> Self {
        Self { strategies }
    }

    /// Find the first strategy that matches the given task.
    pub fn find_matching(&self, task: &str) -> Option<&ParadigmStrategy> {
        self.strategies.iter().find(|s| s.matches(task))
    }

    /// Get all strategies.
    pub fn all_strategies(&self) -> &[ParadigmStrategy] {
        &self.strategies
    }
}

impl Default for ParadigmStrategyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paradigm_strategy_match() {
        let strategy = ParadigmStrategy {
            trigger_pattern: "refactor|rewrite|restructure".to_string(),
            paradigm_sequence: vec![DomainParadigmKind::Plan, DomainParadigmKind::ReAct, DomainParadigmKind::Reflect],
            sub_agent_types: Vec::new(),
            description: "Refactoring tasks".to_string(),
        };

        assert!(strategy.matches("Please refactor the auth module"));
        assert!(strategy.matches("I want to rewrite the login handler"));
        assert!(!strategy.matches("What does this function do?"));
    }

    #[test]
    fn test_sub_agent_type_definition() {
        let definition = SubAgentTypeDefinition {
            name: "searcher".to_string(),
            description: "Explores codebase with read+grep+glob".to_string(),
            system_prompt: "You are a code exploration agent...".to_string(),
            available_tools: vec!["read_file".to_string(), "grep".to_string(), "glob".to_string()],
            permission_threshold: PermissionLevel::Read,
            budget: 40_000,
            modifies_files: false,
            merge_strategy: SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
        };

        assert_eq!(definition.name, "searcher");
        assert_eq!(definition.available_tools.len(), 3);
        assert!(!definition.modifies_files);
        assert_eq!(definition.budget, 40_000);
    }

    #[test]
    fn test_sub_agent_type_definition_defaults() {
        let defaults = SubAgentTypeDefinition::defaults();
        assert_eq!(defaults.len(), 4);

        // Code agent modifies files and uses Merge strategy
        let code = defaults.iter().find(|d| d.name == "code").unwrap();
        assert!(code.modifies_files);
        assert_eq!(code.merge_strategy, SubAgentMergeStrategy::Merge);

        // Explore agent doesn't modify files
        let explore = defaults.iter().find(|d| d.name == "explore").unwrap();
        assert!(!explore.modifies_files);
    }

    #[test]
    fn test_sub_agent_merge_strategy_serialization() {
        let strategy = SubAgentMergeStrategy::Merge;
        let json = serde_json::to_string(&strategy).unwrap();
        assert_eq!(json, "\"Merge\"");

        let parsed: SubAgentMergeStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SubAgentMergeStrategy::Merge);
    }

    #[test]
    fn test_strategy_registry() {
        let strategies = vec![
            ParadigmStrategy {
                trigger_pattern: "refactor|rewrite".to_string(),
                paradigm_sequence: vec![DomainParadigmKind::Plan, DomainParadigmKind::ReAct],
                sub_agent_types: Vec::new(),
                description: "Refactoring".to_string(),
            },
            ParadigmStrategy {
                trigger_pattern: "find|search|understand".to_string(),
                paradigm_sequence: vec![DomainParadigmKind::Explore],
                sub_agent_types: Vec::new(),
                description: "Search".to_string(),
            },
        ];

        let registry = ParadigmStrategyRegistry::from_strategies(strategies);

        let match_refactor = registry.find_matching("refactor the auth module");
        assert!(match_refactor.is_some());
        assert_eq!(match_refactor.unwrap().paradigm_sequence.len(), 2);

        let match_search = registry.find_matching("find all uses of authenticate");
        assert!(match_search.is_some());
        assert_eq!(match_search.unwrap().paradigm_sequence.len(), 1);

        let no_match = registry.find_matching("tell me a joke");
        assert!(no_match.is_none());
    }
}
