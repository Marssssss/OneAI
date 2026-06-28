//! ResearchPack — the research domain configuration pack.
//!
//! ResearchPack is the second concrete DomainPack implementation, designed for
//! research-oriented tasks: web search, information gathering, analysis, and
//! synthesis. It's modeled after OpenCode's Skill-based research agent pattern
//! and Claude Code's WebSearch/WebFetch workflow.
//!
//! Key differences from CodingPack:
//! - **Web-centric tools**: WebSearch + WebFetch are primary tools (not just supplements)
//! - **Read-only emphasis**: No file editing tools — research agents read and search,
//!   they don't modify the environment
//! - **Research context sources**: DateSource is high-priority (research needs
//!   current date for time-sensitive queries)
//! - **Relaxed permissions**: All tools are auto-approved (research is read-only)
//! - **Research paradigm strategies**: Search → Analyze → Synthesize → Report
//! - **Research compression template**: Preserves search queries, key findings,
//!   source citations, and synthesis conclusions
//!
//! Use ResearchPack when:
//! - The agent's primary task is information gathering (not code modification)
//! - The agent needs to search the web and synthesize findings
//! - The task involves fact-checking, citation, and report generation

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use oneai_core::PermissionLevel;
use oneai_core::traits::Tool;
use oneai_tool::{
    WebSearchTool, WebFetchTool, FileReadTool, GrepTool, GlobTool,
    FileListTool, EnvironmentTool, CalculatorTool,
};

use oneai_workflow::{WorkflowConfig, StepConfig, StateGraph, GraphNode, GraphEdge, NodeAction, EdgeCondition};

use crate::domain_pack::DomainPack;
use crate::tool_decorator::ToolDecorator;
use crate::permission_profile::{PermissionProfile, DenyPattern};
use crate::paradigm_strategy::{ParadigmStrategy, SubAgentTypeDefinition, DomainParadigmKind};
use crate::compression_template::CompressionTemplate;
use crate::builtin_sources::{
    DateSource, EnvironmentInfoSource, ProjectInstructionsSource,
    ProjectConfigSource,
};

// ─── Research System Prompt ──────────────────────────────────────────────────

/// The research domain system prompt template.
///
/// Defines the research agent's role, capabilities, and behavioral guidelines.
/// Research agents focus on information gathering, analysis, and synthesis —
/// not on modifying the environment.
pub const RESEARCH_SYSTEM_PROMPT: &str = "\
You are an intelligent research agent that can search, gather, analyze, and synthesize \
information from the web and local files. You have access to web search, web fetch, and \
file reading tools, but NO editing capabilities — you are a read-only observer and analyst.

Key principles:
1. **Search first**: For any factual question, start by searching the web. Use multiple \
queries to cross-reference findings from different sources.
2. **Verify sources**: Always fetch the actual source content, don't rely solely on search \
snippets. Verify claims by checking multiple independent sources.
3. **Cite everything**: Every claim in your response must be backed by a source citation \
(URL, document title, or file path). Use [Source: URL/path] notation for citations.
4. **Synthesize**: Don't just list findings — analyze patterns, identify contradictions, \
and draw conclusions. Weigh evidence from multiple sources.
5. **Be current**: Research is time-sensitive. Always check the date of sources and note \
when information may be outdated. Use the current date context provided to you.
6. **Stay focused**: Research tasks can lead to information overload. Stay on-topic and \
prioritize relevance over comprehensiveness.

When you need to search the web, use web_search. When you need to read a specific page, \
use web_fetch. When you need to read local files, use read_file. When your research is \
complete, provide a comprehensive synthesis with citations.";

// ─── Research Sub-Agent Type Definitions ──────────────────────────────────────

/// Sub-agent types available in the research domain.
fn research_sub_agent_types() -> Vec<SubAgentTypeDefinition> {
    vec![
        SubAgentTypeDefinition {
            name: "searcher".to_string(),
            description: "Searches the web and local files to find relevant information on a topic".to_string(),
            system_prompt: "You are a research search agent. Your job is to find relevant information \
                on the given topic using web search and file reading tools. Start with web_search to \
                discover sources, then web_fetch to read the most promising results. Also check local \
                files with read_file if relevant documents exist in the project. Return a comprehensive \
                list of findings with source citations.".to_string(),
            available_tools: vec![
                "web_search".to_string(),
                "web_fetch".to_string(),
                "read_file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "calculator".to_string(),
            ],
            permission_threshold: PermissionLevel::Standard,
            budget: 50_000,
            modifies_files: false,
            merge_strategy: crate::paradigm_strategy::SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
        },
        SubAgentTypeDefinition {
            name: "analyzer".to_string(),
            description: "Analyzes gathered information, identifies patterns, and draws conclusions".to_string(),
            system_prompt: "You are a research analysis agent. Your job is to analyze the gathered \
                information and identify patterns, contradictions, and key insights. You have \
                access to read_file and web_fetch for verification, and calculator for quantitative \
                analysis. Return a structured analysis with key findings, evidence quality ratings, \
                and preliminary conclusions.".to_string(),
            available_tools: vec![
                "read_file".to_string(),
                "web_fetch".to_string(),
                "calculator".to_string(),
                "grep".to_string(),
            ],
            permission_threshold: PermissionLevel::Read,
            budget: 30_000,
            modifies_files: false,
            merge_strategy: crate::paradigm_strategy::SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
        },
        SubAgentTypeDefinition {
            name: "verifier".to_string(),
            description: "Verifies claims by cross-referencing multiple independent sources".to_string(),
            system_prompt: "You are a research verification agent. Your job is to verify claims \
                by checking multiple independent sources. For each claim, search for contradicting \
                evidence and assess the reliability of supporting sources. Return a verification \
                report with confirmed claims, disputed claims, and unverified claims.".to_string(),
            available_tools: vec![
                "web_search".to_string(),
                "web_fetch".to_string(),
                "read_file".to_string(),
            ],
            permission_threshold: PermissionLevel::Standard,
            budget: 25_000,
            modifies_files: false,
            merge_strategy: crate::paradigm_strategy::SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
        },
    ]
}

// ─── ResearchPack Factory ─────────────────────────────────────────────────────

/// Create a ResearchPack DomainPack for the given project directory.
///
/// This is the primary entry point for configuring a research domain agent:
///
/// ```ignore
/// let app = AppBuilder::new()
///     .provider(provider)
///     .domain_pack(research_pack("/project/dir"))  // ← one-line domain switch
///     .build()?;
/// ```
///
/// The ResearchPack provides:
/// - 8 research tools (web_search, web_fetch, read, grep, glob, list, environment, calculator)
/// - Tool decorators with research-specific descriptions
/// - Date/environment/project context sources (high priority for research)
/// - Research permission profile (all tools auto-approved — research is read-only)
/// - Search/analyze/verify paradigm strategies
/// - Research compression template (preserve queries, findings, citations, conclusions)
/// - Research system prompt
pub fn research_pack(project_dir: &str) -> DomainPack {
    DomainPack {
        name: "research".to_string(),
        description: "Research domain pack — provides tools, context, permissions, and strategies for information gathering, analysis, and synthesis tasks".to_string(),

        // Layer 1: Domain-specific tools
        // ResearchPack is web-centric: search and fetch are primary tools.
        // No editing tools — research agents are read-only observers.
        tools: vec![
            Arc::new(WebSearchTool::new()) as Arc<dyn Tool>,
            Arc::new(WebFetchTool::new()) as Arc<dyn Tool>,
            Arc::new(FileReadTool::new()) as Arc<dyn Tool>,
            Arc::new(GrepTool::new()) as Arc<dyn Tool>,
            Arc::new(GlobTool::new()) as Arc<dyn Tool>,
            Arc::new(FileListTool::new()) as Arc<dyn Tool>,
            Arc::new(EnvironmentTool::new()) as Arc<dyn Tool>,
            Arc::new(CalculatorTool::new()) as Arc<dyn Tool>,
        ],

        // Layer 1 supplement: Tool decorators — research-specific descriptions
        tool_decorators: vec![
            ToolDecorator::with_description(
                "web_search",
                "Search the web for information. Returns a list of results with titles, \
                URLs, and snippets. This is your PRIMARY tool for discovering information. \
                Use multiple queries to cross-reference findings. Supports Google, Bing, \
                SerpAPI, and DuckDuckGo (default, free). Always check the date of results \
                for time-sensitive research."
            ),
            ToolDecorator::with_description(
                "web_fetch",
                "Fetch content from a web URL and convert it to structured Markdown. \
                This is your SECONDARY tool for reading discovered sources. Always verify \
                claims by fetching the actual source content — don't rely solely on search \
                snippets. Preserves headings, links, and semantic elements for analysis."
            ),
            ToolDecorator::with_description(
                "read_file",
                "Read local files for project-specific context. Use for: reading documentation, \
                configuration files, research papers stored locally, and project README files. \
                Supports offset+limit for large files."
            ),
            ToolDecorator::with_description(
                "grep",
                "Search file contents using regex patterns. Use for: finding specific information \
                in local documents, searching through research notes, and locating relevant passages."
            ),
            ToolDecorator::with_description(
                "glob",
                "Find files matching glob patterns. Use for: discovering relevant local files, \
                finding research documents, and exploring project structure."
            ),
            ToolDecorator::with_description(
                "list_directory",
                "List directory contents. Use for: exploring project structure to find relevant \
                local documents and research materials."
            ),
            ToolDecorator::with_description(
                "environment",
                "Get environment information: working directory, platform, available tools. \
                Includes current date — CRITICAL for time-sensitive research."
            ),
            ToolDecorator::with_description(
                "calculator",
                "Perform mathematical calculations. Use for: quantitative analysis, statistical \
                computations, and verifying numerical claims in your research."
            ),
        ],

        // Layer 2: Context sources — research environment sensing
        // DateSource is priority 1 (research is time-sensitive)
        // ProjectInstructions is priority 2 (project-specific research context)
        context_sources: vec![
            Arc::new(ProjectInstructionsSource::new(project_dir)),
            Arc::new(DateSource::new()), // High priority — research needs current date
            Arc::new(ProjectConfigSource::new(project_dir)),
            Arc::new(EnvironmentInfoSource::new()),
        ],

        // Layer 3: Permission profile — research permission classification
        // Research is read-only: ALL tools are auto-approved
        // No editing permissions needed — research agents don't modify anything
        permission_profile: PermissionProfile {
            name: "research".to_string(),
            auto_approve: HashSet::from([
                "web_search".to_string(),
                "web_fetch".to_string(),
                "read_file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "list_directory".to_string(),
                "environment".to_string(),
                "calculator".to_string(),
            ]),
            require_confirmation: HashSet::new(), // No tools require confirmation
            deny_by_default: vec![
                // Even in research mode, block dangerous shell commands if somehow accessed
                DenyPattern::deny_tool_args(
                    "shell",
                    ".*",
                    "Shell execution is not available in research mode"
                ),
            ],
            permission_overrides: HashMap::new(),
            default_threshold: PermissionLevel::Standard,
        },

        // Layer 4: Paradigm strategies — research task patterns
        paradigm_strategies: vec![
            // Deep research → Search + Analyze + Verify + Synthesize
            ParadigmStrategy {
                trigger_pattern: "research|investigate|analyze|study|compare|evaluate".to_string(),
                paradigm_sequence: vec![
                    DomainParadigmKind::Explore,   // Search phase
                    DomainParadigmKind::Reflect,    // Analyze & verify phase
                    DomainParadigmKind::Plan,       // Synthesize & report phase
                ],
                sub_agent_types: research_sub_agent_types(),
                description: "Deep research tasks require comprehensive search, analysis, verification, and synthesis".to_string(),
            },
            // Fact-checking → Search + Verify
            ParadigmStrategy {
                trigger_pattern: "verify|fact-check|confirm|check|validate|true|false".to_string(),
                paradigm_sequence: vec![
                    DomainParadigmKind::Explore,   // Search for evidence
                    DomainParadigmKind::Reflect,    // Verify and cross-reference
                ],
                sub_agent_types: vec![research_sub_agent_types()[0].clone(), research_sub_agent_types()[2].clone()],
                description: "Fact-checking requires searching for evidence and verifying from multiple sources".to_string(),
            },
            // Quick lookup → Search only
            ParadigmStrategy {
                trigger_pattern: "what is|who|when|where|how many|definition|meaning".to_string(),
                paradigm_sequence: vec![
                    DomainParadigmKind::Explore,
                ],
                sub_agent_types: vec![research_sub_agent_types()[0].clone()],
                description: "Quick factual lookups use single search phase".to_string(),
            },
            // Summary/synthesis → Analyze + Synthesize
            ParadigmStrategy {
                trigger_pattern: "summarize|synthesize|overview|review|digest|brief".to_string(),
                paradigm_sequence: vec![
                    DomainParadigmKind::Reflect,    // Analyze existing information
                    DomainParadigmKind::Plan,       // Organize into coherent summary
                ],
                sub_agent_types: vec![research_sub_agent_types()[1].clone()],
                description: "Summary/synthesis tasks require analysis and organized reporting".to_string(),
            },
        ],

        // Layer 5: Compression template — research context preservation
        compression_template: CompressionTemplate {
            name: "research".to_string(),
            preserve_fields: vec![
                "search_queries".to_string(),
                "key_findings".to_string(),
                "source_citations".to_string(),
                "verification_status".to_string(),
                "conclusions".to_string(),
                "unanswered_questions".to_string(),
            ],
            template: RESEARCH_COMPRESSION_TEMPLATE.to_string(),
            truncate_rules: HashMap::from([
                ("search_result".to_string(), 500),    // Search snippets truncated to 500 chars
                ("web_content".to_string(), 3000),     // Web content truncated to 3000 chars
                ("file_content".to_string(), 2000),    // File content truncated to 2000 chars
            ]),
            default_variables: HashMap::from([
                ("research_depth".to_string(), "comprehensive".to_string()),
                ("citation_style".to_string(), "[Source: URL/path]".to_string()),
            ]),
        },

        // Layer 7: Memory profile — research memory policy
        memory_profile: crate::memory_profile::MemoryProfile::research(),

        // System prompt
        system_prompt_template: RESEARCH_SYSTEM_PROMPT.to_string(),

        // Layer 6: Predefined workflows and StateGraphs
        workflows: vec![
            deep_research_workflow(),
            literature_review_workflow(),
        ],
        state_graphs: vec![
            research_loop_graph(),
        ],
        sub_agent_definitions: vec![
            // Research domain has a specialized explore sub-agent
            SubAgentTypeDefinition {
                name: "explore".to_string(),
                description: "Research exploration agent".to_string(),
                system_prompt: "You are a research exploration agent. Search and analyze \
                    documents, papers, and web resources to gather relevant information. \
                    Return a comprehensive summary with citations, key findings, and \
                    relevance scores for each source.".to_string(),
                available_tools: vec![
                    "read_file".into(), "grep".into(), "glob".into(),
                    "list_directory".into(), "web_fetch".into(),
                ],
                permission_threshold: oneai_core::PermissionLevel::Read,
                budget: 50_000,
                modifies_files: false,
                merge_strategy: crate::paradigm_strategy::SubAgentMergeStrategy::PreserveOnly,
                structured_output: None,
            },
            SubAgentTypeDefinition::plan(),
            SubAgentTypeDefinition::review(),
        ],
    }
}

// ─── Predefined Workflows (Layer 6) ──────────────────────────────────────────

/// Deep research workflow — systematic research: search → fetch → analyze → synthesize → report.
///
/// A 5-step DAG workflow that:
/// 1. Searches the web for relevant information
/// 2. Fetches the most promising sources (parallel)
/// 3. Analyzes the gathered information
/// 4. Synthesizes findings into coherent conclusions
/// 5. Compiles a comprehensive research report with citations
fn deep_research_workflow() -> WorkflowConfig {
    WorkflowConfig {
        name: "deep-research".to_string(),
        description: "Systematic deep research: search → fetch → analyze → synthesize → report".to_string(),
        version: "1.0".to_string(),
        steps: vec![
            StepConfig {
                id: "search_web".to_string(),
                description: "Search the web for relevant information".to_string(),
                depends_on: vec![],
                tool: Some("web_search".to_string()),
                tool_args: Some(serde_json::json!({"query": "{{research_query}}"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(30),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "fetch_sources".to_string(),
                description: "Fetch content from top sources".to_string(),
                depends_on: vec!["search_web".to_string()],
                tool: Some("web_fetch".to_string()),
                tool_args: Some(serde_json::json!({"url": "{{top_source_url}}"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(60),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "analyze_findings".to_string(),
                description: "Analyze gathered information".to_string(),
                depends_on: vec!["fetch_sources".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Analyze the following research findings. Identify key themes, patterns, contradictions, and gaps in the evidence. Rate the quality and reliability of each source.\n\nSearch results: {{search_web_output}}\nFetched source content: {{fetch_sources_output}}".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "synthesize".to_string(),
                description: "Synthesize findings into conclusions".to_string(),
                depends_on: vec!["analyze_findings".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Based on the analysis: {{analyze_findings_output}}\n\nSynthesize the findings into coherent conclusions. For each conclusion, cite the supporting evidence. Identify areas where evidence is weak or contradictory. Note unanswered questions for further research.".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "compile_report".to_string(),
                description: "Compile research report with citations".to_string(),
                depends_on: vec!["synthesize".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Compile a comprehensive research report from the analysis and synthesis:\n\nAnalysis: {{analyze_findings_output}}\nSynthesis: {{synthesize_output}}\n\nThe report should include:\n1. Executive Summary (key findings in 3-5 bullet points)\n2. Methodology (search queries, sources used)\n3. Findings (organized by theme, with citations)\n4. Conclusions (with evidence ratings)\n5. Recommendations & Further Research (unanswered questions)\n\nUse [Source: URL] notation for all citations.".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ],
        variables: HashMap::new(),
        timeout_secs: Some(300),
        default_retry_policy: oneai_workflow::RetryPolicy::default(),
        continue_on_failure: true, // Research is tolerant of partial failures
    }
}

/// Literature review workflow — systematic literature review: search → screen → analyze → map.
///
/// A 4-step DAG workflow for academic literature review:
/// 1. Search for relevant publications
/// 2. Screen and select relevant sources
/// 3. Analyze each selected source for key insights
/// 4. Map the literature landscape (connections, gaps, trends)
fn literature_review_workflow() -> WorkflowConfig {
    WorkflowConfig {
        name: "literature-review".to_string(),
        description: "Systematic literature review: search → screen → analyze → map".to_string(),
        version: "1.0".to_string(),
        steps: vec![
            StepConfig {
                id: "search_literature".to_string(),
                description: "Search for relevant publications".to_string(),
                depends_on: vec![],
                tool: Some("web_search".to_string()),
                tool_args: Some(serde_json::json!({"query": "{{literature_query}}"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(30),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "screen_sources".to_string(),
                description: "Screen and select relevant sources".to_string(),
                depends_on: vec!["search_literature".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Screen the following search results for relevance to the research topic. For each result, assess:\n1. Relevance to the topic (high/medium/low)\n2. Source credibility (peer-reviewed, institutional, blog, etc.)\n3. Date of publication (is it current enough?)\n4. Key contribution or claim\n\nSelect the top 5-8 most relevant and credible sources for detailed analysis.\n\nSearch results: {{search_literature_output}}".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "analyze_sources".to_string(),
                description: "Analyze each selected source for key insights".to_string(),
                depends_on: vec!["screen_sources".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("For each selected source, perform a detailed analysis:\n\nScreened sources: {{screen_sources_output}}\n\nFor each source:\n1. Main argument or finding\n2. Methodology used\n3. Key evidence and data points\n4. Limitations and criticisms\n5. Connections to other sources in the review\n\nFocus on extracting the unique contribution of each source.".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "map_literature".to_string(),
                description: "Map the literature landscape".to_string(),
                depends_on: vec!["analyze_sources".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Based on the source analyses: {{analyze_sources_output}}\n\nCreate a literature landscape map:\n1. **Thematic clusters**: Group sources by shared themes or approaches\n2. **Chronological trends**: Identify how understanding has evolved over time\n3. **Methodological patterns**: Common methods and their strengths/weaknesses\n4. **Convergence points**: Where multiple sources agree\n5. **Divergence points**: Where sources disagree or contradict\n6. **Gaps**: Topics or perspectives not covered by existing literature\n7. **Future directions**: Where the field is heading based on current trends\n\nThis map should serve as a foundation for the research.".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ],
        variables: HashMap::new(),
        timeout_secs: Some(300),
        default_retry_policy: oneai_workflow::RetryPolicy::default(),
        continue_on_failure: true,
    }
}

/// Research loop StateGraph — cyclic search → fetch → analyze → synthesize/end loop.
///
/// This is the iterative research pattern:
/// 1. think: LLM inference — decide what to search/analyze
/// 2. search: Execute a search or fetch action
/// 3. analyze: Analyze the gathered information
/// 4. synthesize: Produce a synthesis of findings (or continue research)
/// 5. end: Final research report with citations
fn research_loop_graph() -> StateGraph {
    let mut graph = StateGraph::new("research-loop", "think");

    // Think node — LLM decides what to research next
    graph.add_node(GraphNode {
        id: "think".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "You are a research agent. Decide whether you need more information \
                 (search/fetch) or can synthesize what you've gathered. \
                 If you need more info, describe what to search for. \
                 If you have enough, provide your synthesis.".to_string()
            ),
            use_streaming: true,
            include_tool_definitions: true,  // P2-2: Send tools so model can decide
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Search node — execute search or fetch tool
    graph.add_node(GraphNode {
        id: "search".to_string(),
        action: NodeAction::ToolCall {
            tool_name: "{{selected_tool}}".to_string(),
            args_template: Some("{{tool_args}}".to_string()),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Analyze node — analyze what was found
    graph.add_node(GraphNode {
        id: "analyze".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "Analyze the search/fetch results. Identify key findings, \
                 assess source quality, note contradictions or gaps. \
                 Decide if you have enough information or need to search more.".to_string()
            ),
            use_streaming: true,
            include_tool_definitions: true,  // P2-2: Send tools for decision-making
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // End node — produce final research report
    graph.add_node(GraphNode {
        id: "end".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "Compile a comprehensive research report with citations. \
                 Include: executive summary, methodology, findings (organized by theme), \
                 conclusions, and areas for further research. Use [Source: URL] for citations.".to_string()
            ),
            use_streaming: true,
            include_tool_definitions: false,  // P2-2: No tools for final report
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Edges
    // think → search (needs more info)
    graph.add_edge(GraphEdge {
        from: "think".to_string(),
        to: "search".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    // think → end (has enough info for final report)
    graph.add_edge(GraphEdge {
        from: "think".to_string(),
        to: "end".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });
    // search → analyze
    graph.add_edge(GraphEdge {
        from: "search".to_string(),
        to: "analyze".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });
    // analyze → think (continue research loop)
    graph.add_edge(GraphEdge {
        from: "analyze".to_string(),
        to: "think".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });

    graph.add_terminal("end".to_string());

    graph
}

// ─── Research Compression Template ───────────────────────────────────────────

/// The research domain compression template.
///
/// Preserves the most critical context for research tasks:
/// - Search queries used (so the agent can refine queries)
/// - Key findings with citations (so the agent can build on previous research)
/// - Verification status (so the agent knows what's confirmed vs unverified)
/// - Conclusions (so the agent can extend existing conclusions)
/// - Unanswered questions (so the agent can focus remaining effort)
pub const RESEARCH_COMPRESSION_TEMPLATE: &str = "\
## Research Progress Summary

### Search Queries Used
{{search_queries}}

### Key Findings
{{key_findings}}

### Source Citations
{{source_citations}}

### Verification Status
{{verification_status}}

### Conclusions
{{conclusions}}

### Unanswered Questions
{{unanswered_questions}}

---
Research depth: {{research_depth}}
Citation style: {{citation_style}}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_research_pack_creation() {
        let pack = research_pack("/tmp/test_project");

        assert_eq!(pack.name, "research");
        assert_eq!(pack.tools.len(), 8);
        assert_eq!(pack.tool_decorators.len(), 8);
        assert_eq!(pack.context_sources.len(), 4);
        assert!(!pack.system_prompt_template.is_empty());
    }

    #[test]
    fn test_research_pack_permission_profile() {
        let pack = research_pack("/tmp/test");

        // All tools auto-approved (research is read-only)
        assert!(pack.permission_profile.auto_approve.contains("web_search"));
        assert!(pack.permission_profile.auto_approve.contains("web_fetch"));
        assert!(pack.permission_profile.auto_approve.contains("read_file"));
        assert!(pack.permission_profile.auto_approve.contains("grep"));
        assert!(pack.permission_profile.auto_approve.contains("calculator"));

        // No tools require confirmation
        assert!(pack.permission_profile.require_confirmation.is_empty());
    }

    #[test]
    fn test_research_pack_paradigm_strategies() {
        let pack = research_pack("/tmp/test");

        assert!(pack.paradigm_strategies.len() >= 4);

        // Deep research strategy
        let research = pack.paradigm_strategies.iter()
            .find(|s| s.trigger_pattern.contains("research"))
            .unwrap();
        assert_eq!(research.paradigm_sequence.len(), 3);

        // Fact-checking strategy
        let fact_check = pack.paradigm_strategies.iter()
            .find(|s| s.trigger_pattern.contains("verify"))
            .unwrap();
        assert_eq!(fact_check.paradigm_sequence.len(), 2);
    }

    #[test]
    fn test_research_pack_compression_template() {
        let pack = research_pack("/tmp/test");

        assert_eq!(pack.compression_template.name, "research");
        assert!(pack.compression_template.preserve_fields.contains(&"search_queries".to_string()));
        assert!(pack.compression_template.preserve_fields.contains(&"key_findings".to_string()));
        assert!(pack.compression_template.preserve_fields.contains(&"source_citations".to_string()));
        assert!(pack.compression_template.truncate_rules.contains_key("search_result"));
    }

    #[test]
    fn test_research_pack_strategy_matching() {
        let pack = research_pack("/tmp/test");

        // Should match research tasks
        let research_match = pack.paradigm_strategies.iter()
            .find(|s| s.matches("Research the impact of AI on healthcare"));
        assert!(research_match.is_some());

        // Should match fact-checking tasks
        let fact_match = pack.paradigm_strategies.iter()
            .find(|s| s.matches("Verify whether climate change causes more hurricanes"));
        assert!(fact_match.is_some());

        // Should match quick lookup tasks
        let lookup_match = pack.paradigm_strategies.iter()
            .find(|s| s.matches("What is the capital of France"));
        assert!(lookup_match.is_some());

        // Should match synthesis tasks
        let synthesis_match = pack.paradigm_strategies.iter()
            .find(|s| s.matches("Summarize the research on quantum computing"));
        assert!(synthesis_match.is_some());
    }
}
