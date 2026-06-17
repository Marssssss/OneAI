//! DomainPack — the core domain configuration structure.
//!
//! A DomainPack encapsulates the 6 layers of domain-specific workflow embedding:
//! 1. Tools + ToolDecorators: domain-specific tool set and description overrides
//! 2. ContextSources: domain-specific environment sensing with refresh policies
//! 3. PermissionProfile: domain-specific permission classification
//! 4. ParadigmStrategies: domain-specific task → paradigm mapping
//! 5. CompressionTemplate: domain-specific context preservation priorities
//! 6. Workflows + StateGraphs: domain-specific predefined workflows and cyclic graphs
//!
//! The DomainPack is the central unit of domain configuration. It's what
//! you pass to `AppBuilder::domain_pack()` to switch the agent's domain.
//!
//! Usage:
//! ```ignore
//! let app = AppBuilder::new()
//!     .provider(provider)
//!     .domain_pack(coding_pack("/project/dir"))  // ← one-line domain switch
//!     .build()?;
//! ```

use std::sync::Arc;

use oneai_core::traits::Tool;

use crate::context_source::ContextSource;
use crate::permission_profile::PermissionProfile;
use crate::paradigm_strategy::ParadigmStrategy;
use crate::compression_template::CompressionTemplate;
use crate::tool_decorator::ToolDecorator;
use crate::paradigm_strategy::{SubAgentTypeDefinition, SubAgentMergeStrategy};

// ─── DomainPack ────────────────────────────────────────────────────────────────

/// A domain configuration pack — the central unit of domain workflow embedding.
///
/// Contains the complete configuration needed for an agent to operate in
/// a specific domain (coding, research, data analysis, IoT control, etc.).
///
/// The key insight from the design doc:
/// > "领域知识 = 工具集描述 + 上下文注入规则 + 权限分级配置 + 范式策略选择 + 上下文压缩优先级"
///
/// These 5 layers are *configuration*, not hardcoded logic. OneAI makes
/// them declarative, pluggable, and composable via DomainPack.
///
/// DomainPacks can be combined (mixed domain support) — e.g., coding + research
/// for an agent that both writes code and searches documentation. The merge
/// logic (in merge.rs) handles combining multiple packs correctly.
pub struct DomainPack {
    /// Unique domain name (e.g., "coding", "research", "data_analysis").
    pub name: String,

    /// Human-readable description of this domain pack.
    pub description: String,

    /// Domain-specific tools.
    ///
    /// These are registered into the ToolRegistry when the DomainPack is activated.
    /// They become available to the agent for this domain's tasks.
    pub tools: Vec<Arc<dyn Tool>>,

    /// Tool decorators — override descriptions/permissions for base tools.
    ///
    /// When a DomainPack includes a ToolDecorator for "read_file", the tool
    /// definition built for the LLM uses the decorator's description instead
    /// of the base tool's description. This is the primary mechanism for
    /// domain-specific workflow embedding.
    pub tool_decorators: Vec<ToolDecorator>,

    /// Domain-specific context sources.
    ///
    /// These are injected into the conversation as system messages, providing
    /// domain-relevant environment information (git status, file tree, etc.).
    /// Each source has its own refresh policy determining when it updates.
    pub context_sources: Vec<Arc<dyn ContextSource>>,

    /// Domain-specific permission profile.
    ///
    /// Determines how tool calls are approved/denied in this domain context.
    /// Overrides the individual tool's risk_level() with domain-specific rules.
    pub permission_profile: PermissionProfile,

    /// Domain-specific paradigm strategies.
    ///
    /// Maps user task patterns to paradigm sequences and sub-agent configurations.
    /// When the user's task matches a trigger_pattern, the agent applies the
    /// corresponding paradigm sequence.
    pub paradigm_strategies: Vec<ParadigmStrategy>,

    /// Domain-specific compression template.
    ///
    /// When the conversation exceeds the token budget, this template determines
    /// what information is preserved and how the summary is structured.
    /// Different domains have different preservation priorities.
    pub compression_template: CompressionTemplate,

    /// Domain system prompt template.
    ///
    /// The system prompt that defines the agent's role and capabilities in
    /// this domain. When a DomainPack is active, this replaces the default
    /// generic system prompt in AgentLoopConfig.
    pub system_prompt_template: String,

    // ─── Layer 6: Predefined workflows ──────────────────────────────────────

    /// Domain-specific predefined WorkflowDag configurations.
    ///
    /// These are declared in the DomainPack and can be executed via
    /// the `/wf run <name>` CLI command. They provide deterministic
    /// step-by-step workflows for common domain tasks.
    ///
    /// Example: code-review, debug, refactor, test workflows in CodingPack.
    pub workflows: Vec<oneai_workflow::WorkflowConfig>,

    /// Domain-specific predefined StateGraph configurations.
    ///
    /// These are cyclic graph definitions for iterative agent patterns
    /// like ReAct loops. They can be visualized via `/wf graph <name>`
    /// and executed via `/wf run <name>` (for StateGraph-based workflows).
    ///
    /// Example: react-loop StateGraph in CodingPack.
    pub state_graphs: Vec<oneai_workflow::StateGraph>,

    /// Domain-specific sub-agent type definitions.
    ///
    /// These define how sub-agents of different kinds are configured
    /// when spawned by the main agent loop during delegation.
    /// Each SubAgentTypeDefinition specifies the system prompt,
    /// tool whitelist, budget, isolation policy, and optional
    /// structured output validation for a sub-agent kind.
    ///
    /// When a DomainPack is active, these definitions override
    /// the default SubAgentKind configurations. This enables
    /// domain-specific agent roles — e.g., a CodingPack's "code"
    /// sub-agent has a coding-focused prompt and edit tools,
    /// while a ResearchPack's "explore" sub-agent has search tools.
    ///
    /// If a sub-agent kind is not defined here, the DefaultSubAgentFactory
    /// falls back to the kind's default configuration.
    pub sub_agent_definitions: Vec<SubAgentTypeDefinition>,
}

impl DomainPack {
    /// Look up a sub-agent definition by kind name.
    ///
    /// Returns the first definition whose `kind` matches the given name
    /// (case-insensitive comparison). Returns None if no definition is
    /// found for this kind.
    ///
    /// The `DefaultSubAgentFactory` calls this method before creating
    /// a sub-agent. If a definition is found, it uses the definition's
    /// configuration (system_prompt, available_tools, etc.). If not,
    /// it falls back to the SubAgentKind's default configuration.
    pub fn get_sub_agent_definition(&self, kind: &str) -> Option<&SubAgentTypeDefinition> {
        self.sub_agent_definitions.iter()
            .find(|d| d.name.eq_ignore_ascii_case(kind))
    }
}

// Manual Debug impl — dyn Tool and dyn ContextSource don't implement Debug
impl std::fmt::Debug for DomainPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainPack")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("tools_count", &self.tools.len())
            .field("tool_decorators", &self.tool_decorators)
            .field("context_sources_count", &self.context_sources.len())
            .field("permission_profile", &self.permission_profile)
            .field("paradigm_strategies", &self.paradigm_strategies)
            .field("compression_template", &self.compression_template)
            .field("system_prompt_template", &self.system_prompt_template)
            .field("workflows_count", &self.workflows.len())
            .field("state_graphs_count", &self.state_graphs.len())
            .field("sub_agent_definitions_count", &self.sub_agent_definitions.len())
            .finish()
    }
}

// ─── DomainPackBuilder ─────────────────────────────────────────────────────────

/// Builder for constructing DomainPacks.
///
/// Provides a fluent API for assembling domain packs piece by piece.
pub struct DomainPackBuilder {
    name: String,
    description: String,
    tools: Vec<Arc<dyn Tool>>,
    tool_decorators: Vec<ToolDecorator>,
    context_sources: Vec<Arc<dyn ContextSource>>,
    permission_profile: PermissionProfile,
    paradigm_strategies: Vec<ParadigmStrategy>,
    compression_template: CompressionTemplate,
    system_prompt_template: String,
    workflows: Vec<oneai_workflow::WorkflowConfig>,
    state_graphs: Vec<oneai_workflow::StateGraph>,
    sub_agent_definitions: Vec<SubAgentTypeDefinition>,
}

impl DomainPackBuilder {
    /// Start building a DomainPack with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
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

    /// Set the description.
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Add a tool.
    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Add multiple tools.
    pub fn tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    /// Add a tool decorator.
    pub fn tool_decorator(mut self, decorator: ToolDecorator) -> Self {
        self.tool_decorators.push(decorator);
        self
    }

    /// Add multiple tool decorators.
    pub fn tool_decorators(mut self, decorators: Vec<ToolDecorator>) -> Self {
        self.tool_decorators.extend(decorators);
        self
    }

    /// Add a context source.
    pub fn context_source(mut self, source: Arc<dyn ContextSource>) -> Self {
        self.context_sources.push(source);
        self
    }

    /// Add multiple context sources.
    pub fn context_sources(mut self, sources: Vec<Arc<dyn ContextSource>>) -> Self {
        self.context_sources.extend(sources);
        self
    }

    /// Set the permission profile.
    pub fn permission_profile(mut self, profile: PermissionProfile) -> Self {
        self.permission_profile = profile;
        self
    }

    /// Add a paradigm strategy.
    pub fn paradigm_strategy(mut self, strategy: ParadigmStrategy) -> Self {
        self.paradigm_strategies.push(strategy);
        self
    }

    /// Add multiple paradigm strategies.
    pub fn paradigm_strategies(mut self, strategies: Vec<ParadigmStrategy>) -> Self {
        self.paradigm_strategies.extend(strategies);
        self
    }

    /// Set the compression template.
    pub fn compression_template(mut self, template: CompressionTemplate) -> Self {
        self.compression_template = template;
        self
    }

    /// Set the system prompt template.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt_template = prompt.into();
        self
    }

    /// Add a predefined workflow configuration.
    pub fn workflow(mut self, workflow: oneai_workflow::WorkflowConfig) -> Self {
        self.workflows.push(workflow);
        self
    }

    /// Add multiple predefined workflow configurations.
    pub fn workflows(mut self, workflows: Vec<oneai_workflow::WorkflowConfig>) -> Self {
        self.workflows.extend(workflows);
        self
    }

    /// Add a predefined StateGraph configuration.
    pub fn state_graph(mut self, graph: oneai_workflow::StateGraph) -> Self {
        self.state_graphs.push(graph);
        self
    }

    /// Add multiple predefined StateGraph configurations.
    pub fn state_graphs(mut self, graphs: Vec<oneai_workflow::StateGraph>) -> Self {
        self.state_graphs.extend(graphs);
        self
    }

    /// Add a sub-agent type definition.
    pub fn sub_agent_definition(mut self, definition: SubAgentTypeDefinition) -> Self {
        self.sub_agent_definitions.push(definition);
        self
    }

    /// Add multiple sub-agent type definitions.
    pub fn sub_agent_definitions(mut self, definitions: Vec<SubAgentTypeDefinition>) -> Self {
        self.sub_agent_definitions.extend(definitions);
        self
    }

    /// Use the standard sub-agent definitions (plan, explore, code, review).
    pub fn default_sub_agent_definitions(self) -> Self {
        self.sub_agent_definitions(SubAgentTypeDefinition::defaults())
    }

    /// Build the DomainPack.
    pub fn build(self) -> DomainPack {
        DomainPack {
            name: self.name,
            description: self.description,
            tools: self.tools,
            tool_decorators: self.tool_decorators,
            context_sources: self.context_sources,
            permission_profile: self.permission_profile,
            paradigm_strategies: self.paradigm_strategies,
            compression_template: self.compression_template,
            system_prompt_template: self.system_prompt_template,
            workflows: self.workflows,
            state_graphs: self.state_graphs,
            sub_agent_definitions: self.sub_agent_definitions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_tool::CalculatorTool;
    use crate::permission_profile::PermissionProfile;
    use crate::compression_template::CompressionTemplate;

    #[test]
    fn test_domain_pack_builder() {
        let pack = DomainPackBuilder::new("test_domain")
            .description("A test domain pack")
            .tool(Arc::new(CalculatorTool::new()))
            .permission_profile(PermissionProfile::new("test"))
            .compression_template(CompressionTemplate::new("test"))
            .system_prompt("You are a test domain agent.")
            .build();

        assert_eq!(pack.name, "test_domain");
        assert_eq!(pack.description, "A test domain pack");
        assert_eq!(pack.tools.len(), 1);
        assert_eq!(pack.system_prompt_template, "You are a test domain agent.");
    }

    #[test]
    fn test_sub_agent_type_definition_defaults() {
        let defs = SubAgentTypeDefinition::defaults();
        assert_eq!(defs.len(), 4);

        // Check each kind
        assert_eq!(defs[0].name, "plan");
        assert!(!defs[0].modifies_files);
        assert_eq!(defs[1].name, "explore");
        assert!(!defs[1].modifies_files);
        assert_eq!(defs[2].name, "code");
        assert!(defs[2].modifies_files);
        assert_eq!(defs[3].name, "review");
        assert!(!defs[3].modifies_files);
    }

    #[test]
    fn test_sub_agent_type_definition_custom() {
        let def = SubAgentTypeDefinition::custom(
            "security_review",
            "Security-focused code review",
            "You are a security-focused reviewer.",
            vec!["read_file".into(), "grep".into()],
            20_000,
            false,
        );
        assert_eq!(def.name, "security_review");
        assert!(!def.modifies_files);
        assert_eq!(def.merge_strategy, SubAgentMergeStrategy::PreserveOnly);
    }

    #[test]
    fn test_domain_pack_with_sub_agent_definitions() {
        let pack = DomainPackBuilder::new("coding")
            .description("Coding domain")
            .sub_agent_definitions(SubAgentTypeDefinition::defaults())
            .build();

        assert_eq!(pack.sub_agent_definitions.len(), 4);

        // Lookup by kind name
        let code_def = pack.get_sub_agent_definition("code").unwrap();
        assert!(code_def.modifies_files);
        assert_eq!(code_def.merge_strategy, SubAgentMergeStrategy::Merge);

        // Lookup should be case-insensitive
        let explore_def = pack.get_sub_agent_definition("Explore").unwrap();
        assert_eq!(explore_def.name, "explore");

        // Unknown kind → None
        assert!(pack.get_sub_agent_definition("unknown").is_none());
    }

    #[test]
    fn test_domain_pack_builder_custom_sub_agent() {
        let custom_def = SubAgentTypeDefinition::custom(
            "security_audit",
            "Security audit agent",
            "Audit for security vulnerabilities.",
            vec!["read_file".into(), "grep".into(), "glob".into()],
            25_000,
            false,
        );

        let pack = DomainPackBuilder::new("security")
            .description("Security domain")
            .sub_agent_definition(custom_def)
            .build();

        assert_eq!(pack.sub_agent_definitions.len(), 1);
        let def = pack.get_sub_agent_definition("security_audit").unwrap();
        assert_eq!(def.name, "security_audit");
    }

    #[test]
    fn test_sub_agent_merge_strategy_equality() {
        assert_eq!(SubAgentMergeStrategy::Merge, SubAgentMergeStrategy::Merge);
        assert_ne!(SubAgentMergeStrategy::Merge, SubAgentMergeStrategy::Rebase);
        assert_ne!(SubAgentMergeStrategy::Merge, SubAgentMergeStrategy::PreserveOnly);
    }
}
