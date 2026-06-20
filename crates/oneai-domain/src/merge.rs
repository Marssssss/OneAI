//! MergedDomainPack — logic for combining multiple DomainPacks.
//!
//! When multiple DomainPacks are activated together (e.g., coding + research
//! for an agent that writes code and searches documentation), their
//! configurations need to be combined correctly:
//!
//! - Tools: union (deduplicated by name)
//! - ToolDecorators: union (later pack overrides earlier for same tool_name)
//! - ContextSources: union (all sources inject their information)
//! - PermissionProfile: strictest wins (require_confirmation beats auto_approve)
//! - ParadigmStrategies: union (deduplicated by trigger_pattern)
//! - CompressionTemplate: use the primary pack (first pack in the list)
//! - System prompt: concatenate with section headers
//! - Workflows: union (deduplicated by name)
//! - StateGraphs: union (deduplicated by name)

use std::sync::Arc;

use oneai_core::traits::Tool;

use crate::domain_pack::DomainPack;
use crate::paradigm_strategy::SubAgentTypeDefinition;
use crate::context_source::ContextSource;
use crate::permission_profile::PermissionProfile;
use crate::paradigm_strategy::ParadigmStrategy;
use crate::compression_template::CompressionTemplate;
use crate::tool_decorator::ToolDecorator;

// ─── MergedDomainPack ──────────────────────────────────────────────────────────

/// The result of merging multiple DomainPacks.
///
/// Has the same structure as DomainPack but represents the combined
/// configuration of all active domain packs. The merge logic ensures
/// that the combined configuration is correct and consistent:
///
/// - Tools from all packs are available (union, deduplicated by name)
/// - The strictest permission rules are applied (safety first)
/// - All context sources inject their information (full visibility)
/// - The primary pack's compression template is used (consistent behavior)
/// - System prompts are concatenated (multi-domain awareness)
pub struct MergedDomainPack {
    /// The merged domain name (concatenation of all pack names).
    pub name: String,

    /// Human-readable description of the merged configuration.
    pub description: String,

    /// Merged tools (union of all packs, deduplicated by name).
    pub tools: Vec<Arc<dyn Tool>>,

    /// Merged tool decorators (union, later packs override earlier for same tool_name).
    pub tool_decorators: Vec<ToolDecorator>,

    /// Merged context sources (union of all packs' sources).
    pub context_sources: Vec<Arc<dyn ContextSource>>,

    /// Merged permission profile (strictest wins).
    pub permission_profile: PermissionProfile,

    /// Merged paradigm strategies (union, deduplicated by trigger_pattern).
    pub paradigm_strategies: Vec<ParadigmStrategy>,

    /// Compression template from the primary pack (first pack in the list).
    pub compression_template: CompressionTemplate,

    /// Merged system prompt (concatenated with section headers).
    pub system_prompt_template: String,

    /// Merged predefined workflows (union, deduplicated by name).
    pub workflows: Vec<oneai_workflow::WorkflowConfig>,

    /// Merged predefined StateGraphs (union, deduplicated by name).
    pub state_graphs: Vec<oneai_workflow::StateGraph>,

    /// Merged sub-agent type definitions (union, later pack overrides earlier for same kind).
    pub sub_agent_definitions: Vec<SubAgentTypeDefinition>,
}

impl MergedDomainPack {
    /// Merge multiple DomainPacks into a single MergedDomainPack.
    ///
    /// The merge logic follows these rules:
    ///
    /// | Dimension | Merge Rule |
    /// |-----------|-----------|
    /// | Tools | Union, deduplicated by name |
    /// | Decorators | Union, later pack overrides earlier |
    /// | Context sources | Union (all inject) |
    /// | PermissionProfile | Strictest wins |
    /// | ParadigmStrategies | Union, deduplicated by trigger_pattern |
    /// | CompressionTemplate | Primary pack (first) |
    /// | System prompt | Concatenated with section headers |
    pub fn merge(packs: Vec<DomainPack>) -> Self {
        if packs.is_empty() {
            return Self::empty();
        }

        if packs.len() == 1 {
            // Single pack — no merge needed, just convert
            let pack = &packs[0];
            return Self {
                name: pack.name.clone(),
                description: pack.description.clone(),
                tools: pack.tools.clone(),
                tool_decorators: pack.tool_decorators.clone(),
                context_sources: pack.context_sources.clone(),
                permission_profile: pack.permission_profile.clone(),
                paradigm_strategies: pack.paradigm_strategies.clone(),
                compression_template: pack.compression_template.clone(),
                system_prompt_template: pack.system_prompt_template.clone(),
                workflows: pack.workflows.clone(),
                state_graphs: pack.state_graphs.clone(),
                sub_agent_definitions: pack.sub_agent_definitions.clone(),
            };
        }

        // Multiple packs — apply merge logic

        // Name: concatenate pack names with "+"
        let name = packs.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join("+");

        // Description: concatenate descriptions
        let description = packs.iter()
            .map(|p| format!("{}: {}", p.name, p.description))
            .collect::<Vec<_>>()
            .join("; ");

        // Tools: union, deduplicated by name
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut seen_names = std::collections::HashSet::new();
        for pack in &packs {
            for tool in &pack.tools {
                let name = tool.name();
                if !seen_names.contains(name) {
                    seen_names.insert(name.to_string());
                    tools.push(tool.clone());
                }
            }
        }

        // ToolDecorators: union, later pack overrides earlier for same tool_name
        let mut decorator_map: std::collections::HashMap<String, ToolDecorator> = std::collections::HashMap::new();
        for pack in &packs {
            for decorator in &pack.tool_decorators {
                decorator_map.insert(decorator.tool_name.clone(), decorator.clone());
            }
        }
        let tool_decorators = decorator_map.values().cloned().collect();

        // ContextSources: union (all inject)
        let mut context_sources: Vec<Arc<dyn ContextSource>> = Vec::new();
        let mut seen_keys = std::collections::HashSet::new();
        for pack in &packs {
            for source in &pack.context_sources {
                if !seen_keys.contains(source.key()) {
                    seen_keys.insert(source.key().to_string());
                    context_sources.push(source.clone());
                }
            }
        }

        // PermissionProfile: strictest wins (iteratively merge)
        let permission_profile = packs.iter()
            .skip(1)
            .fold(packs[0].permission_profile.clone(), |acc, pack| {
                PermissionProfile::merge_strictest(&acc, &pack.permission_profile)
            });

        // ParadigmStrategies: union, deduplicated by trigger_pattern
        let mut strategy_map: std::collections::HashMap<String, ParadigmStrategy> = std::collections::HashMap::new();
        for pack in &packs {
            for strategy in &pack.paradigm_strategies {
                if !strategy_map.contains_key(&strategy.trigger_pattern) {
                    strategy_map.insert(strategy.trigger_pattern.clone(), strategy.clone());
                }
            }
        }
        let paradigm_strategies = strategy_map.values().cloned().collect();

        // CompressionTemplate: use primary pack (first)
        let compression_template = packs[0].compression_template.clone();

        // System prompt: concatenate with section headers
        let system_prompt_template = if packs.len() == 1 {
            packs[0].system_prompt_template.clone()
        } else {
            packs.iter()
                .map(|p| format!("## {} Domain\n{}", p.name, p.system_prompt_template))
                .collect::<Vec<_>>()
                .join("\n\n")
        };

        // Workflows: union, deduplicated by name
        let mut workflow_map: std::collections::HashMap<String, oneai_workflow::WorkflowConfig> = std::collections::HashMap::new();
        for pack in &packs {
            for workflow in &pack.workflows {
                if !workflow_map.contains_key(&workflow.name) {
                    workflow_map.insert(workflow.name.clone(), workflow.clone());
                }
            }
        }
        let workflows = workflow_map.values().cloned().collect();

        // StateGraphs: union, deduplicated by name
        let mut graph_map: std::collections::HashMap<String, oneai_workflow::StateGraph> = std::collections::HashMap::new();
        for pack in &packs {
            for graph in &pack.state_graphs {
                if !graph_map.contains_key(&graph.name) {
                    graph_map.insert(graph.name.clone(), graph.clone());
                }
            }
        }
        let state_graphs = graph_map.values().cloned().collect();

        // SubAgentDefinitions: union, later pack overrides earlier for same kind
        let mut sub_agent_map: std::collections::HashMap<String, SubAgentTypeDefinition> = std::collections::HashMap::new();
        for pack in &packs {
            for definition in &pack.sub_agent_definitions {
                // Later pack's definition overrides earlier for same kind
                sub_agent_map.insert(definition.name.clone(), definition.clone());
            }
        }
        let sub_agent_definitions = sub_agent_map.values().cloned().collect();

        Self {
            name,
            description,
            tools,
            tool_decorators,
            context_sources,
            permission_profile,
            paradigm_strategies,
            compression_template,
            system_prompt_template,
            workflows,
            state_graphs,
            sub_agent_definitions,
        }
    }

    /// Create an empty MergedDomainPack (used when no DomainPacks are configured).
    ///
    /// When no DomainPack is active, the agent falls back to its default behavior
    /// — generic system prompt, tool-level risk classification, etc.
    pub fn empty() -> Self {
        Self {
            name: "default".to_string(),
            description: "No domain pack configured — using default settings".to_string(),
            tools: Vec::new(),
            tool_decorators: Vec::new(),
            context_sources: Vec::new(),
            permission_profile: PermissionProfile::default(),
            paradigm_strategies: Vec::new(),
            compression_template: CompressionTemplate::default(),
            system_prompt_template: String::new(),
            workflows: Vec::new(),
            state_graphs: Vec::new(),
            sub_agent_definitions: Vec::new(),
        }
    }

    /// Find a tool decorator for a specific tool name.
    pub fn find_decorator(&self, tool_name: &str) -> Option<&ToolDecorator> {
        self.tool_decorators.iter().find(|d| d.tool_name == tool_name)
    }

    /// Find the paradigm strategy that matches a given task.
    pub fn find_matching_strategy(&self, task: &str) -> Option<&ParadigmStrategy> {
        self.paradigm_strategies.iter().find(|s| s.matches(task))
    }

    /// Resolve permission for a tool call using the merged permission profile.
    pub fn resolve_permission(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> crate::permission_profile::PermissionAction {
        self.permission_profile.resolve(tool_name, args)
    }

    /// Check if the merged pack has any tools.
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// Check if the merged pack has any context sources.
    pub fn has_context_sources(&self) -> bool {
        !self.context_sources.is_empty()
    }

    /// Get a predefined workflow configuration by name.
    pub fn get_workflow_config(&self, name: &str) -> Option<&oneai_workflow::WorkflowConfig> {
        self.workflows.iter().find(|w| w.name == name)
    }

    /// Get a predefined StateGraph by name.
    pub fn get_state_graph(&self, name: &str) -> Option<&oneai_workflow::StateGraph> {
        self.state_graphs.iter().find(|g| g.name == name)
    }

    /// Get all available workflow names.
    pub fn workflow_names(&self) -> Vec<String> {
        self.workflows.iter().map(|w| w.name.clone()).collect()
    }

    /// Get all available StateGraph names.
    pub fn state_graph_names(&self) -> Vec<String> {
        self.state_graphs.iter().map(|g| g.name.clone()).collect()
    }

    /// Look up a sub-agent definition by kind name.
    ///
    /// Returns the first definition whose `kind` matches the given name
    /// (case-insensitive). Returns None if no definition is found.
    pub fn get_sub_agent_definition(&self, kind: &str) -> Option<&SubAgentTypeDefinition> {
        self.sub_agent_definitions.iter()
            .find(|d| d.name.eq_ignore_ascii_case(kind))
    }

    /// Get all available sub-agent kind names.
    pub fn sub_agent_kind_names(&self) -> Vec<String> {
        self.sub_agent_definitions.iter().map(|d| d.name.clone()).collect()
    }

    /// Convert this MergedDomainPack back into a DomainPack.
    ///
    /// Useful when a `DomainPack` is needed by APIs that only accept
    /// `DomainPack` (e.g., `agent_card_from_domain_pack()`).
    pub fn to_domain_pack(&self) -> DomainPack {
        DomainPack {
            name: self.name.clone(),
            description: self.description.clone(),
            tools: self.tools.clone(),
            tool_decorators: self.tool_decorators.clone(),
            context_sources: self.context_sources.clone(),
            permission_profile: self.permission_profile.clone(),
            paradigm_strategies: self.paradigm_strategies.clone(),
            compression_template: self.compression_template.clone(),
            system_prompt_template: self.system_prompt_template.clone(),
            workflows: self.workflows.clone(),
            state_graphs: self.state_graphs.clone(),
            sub_agent_definitions: self.sub_agent_definitions.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::PermissionLevel;
    use oneai_tool::CalculatorTool;
    use crate::permission_profile::PermissionProfile;
    use crate::compression_template::CompressionTemplate;

    fn make_test_pack(name: &str) -> DomainPack {
        DomainPack {
            name: name.to_string(),
            description: format!("{} domain pack", name),
            tools: Vec::new(),
            tool_decorators: Vec::new(),
            context_sources: Vec::new(),
            permission_profile: PermissionProfile::new(name),
            paradigm_strategies: Vec::new(),
            compression_template: CompressionTemplate::new(name),
            system_prompt_template: format!("You are a {} agent.", name),
            workflows: Vec::new(),
            state_graphs: Vec::new(),
            sub_agent_definitions: Vec::new(),
        }
    }

    #[test]
    fn test_merge_single_pack() {
        let pack = make_test_pack("coding");
        let merged = MergedDomainPack::merge(vec![pack]);

        assert_eq!(merged.name, "coding");
        assert_eq!(merged.system_prompt_template, "You are a coding agent.");
    }

    #[test]
    fn test_merge_two_packs() {
        let pack_a = DomainPack {
            name: "coding".to_string(),
            description: "Coding domain".to_string(),
            tools: vec![Arc::new(CalculatorTool::new()) as Arc<dyn Tool>],
            tool_decorators: Vec::new(),
            context_sources: Vec::new(),
            permission_profile: PermissionProfile {
                name: "coding".to_string(),
                auto_approve: std::collections::HashSet::from(["calculator".to_string()]),
                require_confirmation: std::collections::HashSet::from(["shell".to_string()]),
                deny_by_default: Vec::new(),
                permission_overrides: std::collections::HashMap::new(),
                default_threshold: PermissionLevel::Standard,
            },
            paradigm_strategies: Vec::new(),
            compression_template: CompressionTemplate::new("coding"),
            system_prompt_template: "You are a coding agent.".to_string(),
            workflows: Vec::new(),
            state_graphs: Vec::new(),
            sub_agent_definitions: Vec::new(),
        };

        let pack_b = DomainPack {
            name: "research".to_string(),
            description: "Research domain".to_string(),
            tools: Vec::new(),
            tool_decorators: Vec::new(),
            context_sources: Vec::new(),
            permission_profile: PermissionProfile {
                name: "research".to_string(),
                auto_approve: std::collections::HashSet::from(["calculator".to_string(), "web_search".to_string()]),
                require_confirmation: std::collections::HashSet::from(["web_fetch".to_string()]),
                deny_by_default: Vec::new(),
                permission_overrides: std::collections::HashMap::new(),
                default_threshold: PermissionLevel::Read,
            },
            paradigm_strategies: Vec::new(),
            compression_template: CompressionTemplate::new("research"),
            system_prompt_template: "You are a research agent.".to_string(),
            workflows: Vec::new(),
            state_graphs: Vec::new(),
            sub_agent_definitions: Vec::new(),
        };

        let merged = MergedDomainPack::merge(vec![pack_a, pack_b]);

        // Name: concatenated
        assert_eq!(merged.name, "coding+research");

        // Tools: union (calculator from coding)
        assert_eq!(merged.tools.len(), 1);

        // Permission: strictest wins
        // auto_approve: intersection → only "calculator" (in both)
        assert!(merged.permission_profile.auto_approve.contains("calculator"));
        assert!(!merged.permission_profile.auto_approve.contains("web_search"));

        // require_confirmation: union → shell + web_fetch
        assert!(merged.permission_profile.require_confirmation.contains("shell"));
        assert!(merged.permission_profile.require_confirmation.contains("web_fetch"));

        // default_threshold: stricter of Standard and Read → Standard
        assert_eq!(merged.permission_profile.default_threshold, PermissionLevel::Standard);

        // CompressionTemplate: primary pack (coding)
        assert_eq!(merged.compression_template.name, "coding");

        // System prompt: concatenated with headers
        assert!(merged.system_prompt_template.contains("## coding Domain"));
        assert!(merged.system_prompt_template.contains("## research Domain"));
    }

    #[test]
    fn test_merge_empty() {
        let merged = MergedDomainPack::empty();
        assert_eq!(merged.name, "default");
        assert!(merged.tools.is_empty());
        assert!(merged.context_sources.is_empty());
    }

    #[test]
    fn test_merge_tools_dedup() {
        let pack_a = DomainPack {
            name: "a".to_string(),
            tools: vec![Arc::new(CalculatorTool::new()) as Arc<dyn Tool>],
            ..make_test_pack("a")
        };

        let pack_b = DomainPack {
            name: "b".to_string(),
            tools: vec![Arc::new(CalculatorTool::new()) as Arc<dyn Tool>],
            ..make_test_pack("b")
        };

        let merged = MergedDomainPack::merge(vec![pack_a, pack_b]);

        // CalculatorTool deduplicated — only one instance
        assert_eq!(merged.tools.len(), 1);
    }
}
