//! CodingPack — the coding domain configuration pack.
//!
//! CodingPack is the first concrete DomainPack implementation, modeled after
//! Claude Code's workflow embedding mechanism. It provides the complete
//! configuration needed for an agent to operate as a coding assistant:
//!
//! - 8 coding-specific tools (read, edit, write, grep, glob, shell, notebook, environment)
//! - Tool decorators that add coding-specific descriptions to base tools
//! - Context sources for git status, file tree, project config, and environment info
//! - Permission profile that auto-approves read operations, confirms edits/shell,
//!   and denies dangerous commands
//! - Paradigm strategies for refactoring and search tasks
//! - Compression template that preserves file paths, progress, and key decisions
//! - System prompt template for coding assistant behavior

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use oneai_core::PermissionLevel;
use oneai_core::traits::Tool;
use oneai_tool::{
    ShellTool, FileReadTool, FileEditTool, GrepTool, GlobTool,
    FileListTool, NotebookEditTool, EnvironmentTool, WebFetchTool,
    ApplyPatchTool,
};

use oneai_workflow::{WorkflowConfig, StepConfig, StateGraph, GraphNode, GraphEdge, NodeAction, EdgeCondition};

use crate::domain_pack::DomainPack;
use crate::tool_decorator::ToolDecorator;
use crate::permission_profile::{PermissionProfile, DenyPattern};
use crate::paradigm_strategy::{ParadigmStrategy, SubAgentTypeDefinition, DomainParadigmKind};
use crate::compression_template::CompressionTemplate;
use crate::builtin_sources::{
    GitStatusSource, FileTreeSource, ProjectConfigSource, DateSource, EnvironmentInfoSource,
    ProjectInstructionsSource,
};
use crate::repo_map::RepoMapSource;

// ─── Coding System Prompt ──────────────────────────────────────────────────────

/// The coding domain system prompt template.
///
/// Defines the coding agent's role, capabilities, and behavioral guidelines.
/// This replaces the generic system prompt in AgentLoopConfig when CodingPack
/// is active.
pub const CODING_SYSTEM_PROMPT: &str = "\
You are an intelligent coding assistant that can plan, execute, and reflect on \
software development tasks. You have access to tools for reading, editing, searching, \
and executing code in the project.

Key principles:
1. **Read before edit**: Always read the relevant files before making changes. \
Understand the existing code structure before modifying it.
2. **Precise edits**: Use exact string matching for edits. Avoid large rewrites \
when targeted edits suffice.
3. **Test after changes**: After modifying code, run relevant tests to verify \
the changes work correctly.
4. **Incremental progress**: Break complex tasks into smaller steps. Complete \
each step before moving to the next.
5. **Preserve context**: When working on multi-step tasks, keep track of which \
files you've modified and what decisions you've made.

When you need to use a tool, output a tool call. When you have the final answer, \
respond with just text without any tool calls. When a task is complex, you can \
delegate it to a specialized sub-agent or switch to a planning paradigm.";

// ─── Coding Sub-Agent Type Definitions ─────────────────────────────────────────

/// Sub-agent types available in the coding domain.
fn coding_sub_agent_types() -> Vec<SubAgentTypeDefinition> {
    vec![
        SubAgentTypeDefinition {
            name: "searcher".to_string(),
            description: "Explores the codebase to find relevant files, functions, and patterns".to_string(),
            system_prompt: "You are a code exploration agent. Your job is to search and understand \
                the codebase. Use read_file, grep, and glob to find relevant code. Return a \
                comprehensive summary of your findings including file paths, function signatures, \
                and key patterns.".to_string(),
            available_tools: vec![
                "read_file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "list_directory".to_string(),
            ],
            permission_threshold: PermissionLevel::Read,
        },
        SubAgentTypeDefinition {
            name: "coder".to_string(),
            description: "Implements code changes based on a plan or specification".to_string(),
            system_prompt: "You are a code implementation agent. Your job is to write and modify \
                code based on the given specification. Use edit_file and write_file for changes, \
                shell for running tests, and read_file to understand the codebase. Return a \
                summary of all changes you made.".to_string(),
            available_tools: vec![
                "read_file".to_string(),
                "edit_file".to_string(),
                "shell".to_string(),
                "grep".to_string(),
                "glob".to_string(),
            ],
            permission_threshold: PermissionLevel::Standard,
        },
        SubAgentTypeDefinition {
            name: "reviewer".to_string(),
            description: "Reviews code changes for correctness, quality, and potential issues".to_string(),
            system_prompt: "You are a code review agent. Your job is to review code changes for \
                correctness bugs, style issues, and potential improvements. Use read_file to examine \
                the changed code and grep to find related patterns. Return a structured review \
                with findings and suggestions.".to_string(),
            available_tools: vec![
                "read_file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
            ],
            permission_threshold: PermissionLevel::Read,
        },
    ]
}

// ─── CodingPack Factory ────────────────────────────────────────────────────────

/// Create a CodingPack DomainPack for the given project directory.
///
/// This is the primary entry point for configuring a coding domain agent:
///
/// ```ignore
/// let app = AppBuilder::new()
///     .provider(provider)
///     .domain_pack(coding_pack("/project/dir"))  // ← one-line domain switch
///     .build()?;
/// ```
///
/// The CodingPack provides:
/// - 8 coding tools (read, edit, grep, glob, shell, list, notebook, environment)
/// - Tool decorators with coding-specific descriptions
/// - Git/file/config/environment context sources
/// - Coding permission profile (auto-approve read, confirm edit/shell, deny dangerous commands)
/// - Refactoring and search paradigm strategies
/// - Coding compression template (preserve file paths, progress, decisions)
/// - Coding system prompt
pub fn coding_pack(project_dir: &str) -> DomainPack {
    DomainPack {
        name: "coding".to_string(),
        description: "Coding domain pack — provides tools, context, permissions, and strategies for software development tasks".to_string(),

        // Layer 1: Domain-specific tools
        tools: vec![
            Arc::new(FileReadTool::new()) as Arc<dyn Tool>,
            Arc::new(FileEditTool::new()) as Arc<dyn Tool>,
            Arc::new(ApplyPatchTool::new()) as Arc<dyn Tool>,
            Arc::new(ShellTool::new()) as Arc<dyn Tool>,
            Arc::new(GrepTool::new()) as Arc<dyn Tool>,
            Arc::new(GlobTool::new()) as Arc<dyn Tool>,
            Arc::new(FileListTool::new()) as Arc<dyn Tool>,
            Arc::new(NotebookEditTool::new()) as Arc<dyn Tool>,
            Arc::new(EnvironmentTool::new()) as Arc<dyn Tool>,
            Arc::new(WebFetchTool::new()) as Arc<dyn Tool>,
        ],

        // Layer 1 supplement: Tool decorators — coding-specific descriptions
        tool_decorators: vec![
            ToolDecorator::with_description(
                "read_file",
                "Read source code files. Supports offset+limit for large files \
                to avoid overflowing the context window. Returns content with line numbers. \
                For binary files, returns base64-encoded data."
            ),
            ToolDecorator::with_description(
                "edit_file",
                "Perform precise code edits using exact string matching. The old_string \
                must be unique in the file. This is a safe, targeted editing mechanism — \
                avoid large rewrites when targeted edits suffice."
            ),
            ToolDecorator::with_description(
                "apply_patch",
                "Apply a unified diff patch to modify multiple files at once. Use for \
                multi-file refactoring, batch edits, and applying review suggestions. \
                The patch should be in standard unified diff format. Each file's changes \
                are applied atomically — context mismatches skip that file with an error."
            ),
            ToolDecorator::with_description_and_permission(
                "shell",
                "Execute shell commands for compilation, testing, and running scripts. \
                Commands are executed with a timeout (default 120s, max 600s). Dangerous \
                commands (rm -rf, mkfs, etc.) are blocked by default. Use for: cargo build, \
                cargo test, npm test, python scripts, git operations.",
                PermissionLevel::Full
            ),
            ToolDecorator::with_description(
                "grep",
                "Search code content using regex patterns. Recursively searches the project \
                directory for matching lines with file paths and line numbers. Use for: finding \
                function definitions, usages, patterns across the codebase."
            ),
            ToolDecorator::with_description(
                "glob",
                "Find files matching glob patterns (e.g., '**/*.rs', 'src/**/*.toml'). \
                Faster than grep for file discovery. Use for: finding source files, config \
                files, test files."
            ),
            ToolDecorator::with_description(
                "list_directory",
                "List directory contents showing files and subdirectories with sizes. \
                Use for: exploring project structure, finding relevant directories."
            ),
            ToolDecorator::with_description(
                "environment",
                "Get environment information: working directory, platform, shell, \
                available tools. Pure observation — no modifications."
            ),
            ToolDecorator::with_description(
                "web_fetch",
                "Fetch content from a web URL and convert it to structured Markdown. \
                Preserves headings, links, and other semantic elements. Use for: \
                fetching documentation, API references, blog posts, and any web content."
            ),
        ],

        // Layer 2: Context sources — coding environment sensing
        context_sources: vec![
            Arc::new(ProjectInstructionsSource::new(project_dir)), // Highest priority — project instructions
            Arc::new(RepoMapSource::new(project_dir)),            // Structural code summary (priority 8)
            Arc::new(GitStatusSource::new(project_dir)),
            Arc::new(FileTreeSource::new(project_dir)),
            Arc::new(ProjectConfigSource::new(project_dir)),
            Arc::new(DateSource::new()),
            Arc::new(EnvironmentInfoSource::new()),
        ],

        // Layer 3: Permission profile — coding permission classification
        permission_profile: PermissionProfile {
            name: "coding".to_string(),
            auto_approve: HashSet::from([
                "read_file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "list_directory".to_string(),
                "environment".to_string(),
                "calculator".to_string(),
            ]),
            require_confirmation: HashSet::from([
                "edit_file".to_string(),
                "shell".to_string(),
                "write_file".to_string(),
                "notebook_edit".to_string(),
            ]),
            deny_by_default: vec![
                DenyPattern::deny_tool_args(
                    "shell",
                    "rm.*-rf|mkfs|dd.*if=/dev/zero|chmod.*(777|666)",
                    "Irreversible filesystem operations are blocked for safety"
                ),
            ],
            permission_overrides: HashMap::from([
                ("shell".to_string(), PermissionLevel::Full),
            ]),
            default_threshold: PermissionLevel::Standard,
        },

        // Layer 4: Paradigm strategies — coding task patterns
        paradigm_strategies: vec![
            // Refactoring tasks → Plan + ReAct + Reflect
            ParadigmStrategy {
                trigger_pattern: "refactor|rewrite|restructure|redesign".to_string(),
                paradigm_sequence: vec![
                    DomainParadigmKind::Plan,
                    DomainParadigmKind::ReAct,
                    DomainParadigmKind::Reflect,
                ],
                sub_agent_types: coding_sub_agent_types(),
                description: "Complex refactoring/rewrite tasks require planning, execution, and review".to_string(),
            },
            // Bug fixing tasks → Plan + ReAct
            ParadigmStrategy {
                trigger_pattern: "fix|bug|error|crash|broken|debug|issue".to_string(),
                paradigm_sequence: vec![
                    DomainParadigmKind::Plan,
                    DomainParadigmKind::ReAct,
                ],
                sub_agent_types: coding_sub_agent_types(),
                description: "Bug fixing requires understanding the problem and applying targeted fixes".to_string(),
            },
            // Search/understand tasks → Explore
            ParadigmStrategy {
                trigger_pattern: "find|search|understand|explain|how|where|what".to_string(),
                paradigm_sequence: vec![
                    DomainParadigmKind::Explore,
                ],
                sub_agent_types: vec![coding_sub_agent_types()[0].clone()], // Only searcher
                description: "Search and understanding tasks use exploration paradigm".to_string(),
            },
            // Implementation tasks → ReAct
            ParadigmStrategy {
                trigger_pattern: "implement|add|create|build|write|develop".to_string(),
                paradigm_sequence: vec![
                    DomainParadigmKind::ReAct,
                ],
                sub_agent_types: vec![coding_sub_agent_types()[1].clone()], // Only coder
                description: "Implementation tasks use ReAct for iterative development".to_string(),
            },
        ],

        // Layer 5: Compression template — coding context preservation
        compression_template: CompressionTemplate {
            name: "coding".to_string(),
            preserve_fields: vec![
                "critical_files".to_string(),
                "progress_status".to_string(),
                "key_decisions".to_string(),
                "next_steps".to_string(),
                "errors_encountered".to_string(),
            ],
            template: crate::compression_template::CODING_COMPRESSION_TEMPLATE.to_string(),
            truncate_rules: HashMap::from([
                ("tool_output".to_string(), 2000),   // Shell output truncated to 2000 chars
                ("file_content".to_string(), 5000),  // File content truncated to 5000 chars
            ]),
            default_variables: HashMap::from([
                ("constraints".to_string(), "Must maintain backward compatibility".to_string()),
            ]),
        },

        // System prompt
        system_prompt_template: CODING_SYSTEM_PROMPT.to_string(),

        // Layer 6: Predefined workflows and StateGraphs
        workflows: vec![
            code_review_workflow(),
            debug_workflow(),
            refactor_workflow(),
            test_workflow(),
        ],
        state_graphs: vec![
            react_state_graph(),
        ],
    }
}

// ─── Predefined Workflows (Layer 6) ──────────────────────────────────────────

/// Code review workflow — systematic review: diff → check → test → analyze → report.
///
/// A 5-step DAG workflow that:
/// 1. Reads recent code changes (git diff)
/// 2. Checks syntax (cargo check) — parallel with tests and quality review
/// 3. Runs tests (cargo test) — parallel with syntax check and quality review
/// 4. LLM quality analysis — parallel with syntax check and tests
/// 5. Compiles a review report from all results
fn code_review_workflow() -> WorkflowConfig {
    WorkflowConfig {
        name: "code-review".to_string(),
        description: "Systematic code review: diff → check → test → analyze → report".to_string(),
        version: "1.0".to_string(),
        steps: vec![
            StepConfig {
                id: "read_diff".to_string(),
                description: "Read recent code changes".to_string(),
                depends_on: vec![],
                tool: Some("shell".to_string()),
                tool_args: Some(serde_json::json!({"command": "git diff HEAD"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(30),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "check_syntax".to_string(),
                description: "Syntax check".to_string(),
                depends_on: vec!["read_diff".to_string()],
                tool: Some("shell".to_string()),
                tool_args: Some(serde_json::json!({"command": "cargo check 2>&1"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(60),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "run_tests".to_string(),
                description: "Run tests".to_string(),
                depends_on: vec!["read_diff".to_string()],
                tool: Some("shell".to_string()),
                tool_args: Some(serde_json::json!({"command": "cargo test 2>&1 | tail -20"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(120),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "quality_review".to_string(),
                description: "LLM quality review".to_string(),
                depends_on: vec!["read_diff".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Review the following code changes for correctness, style, and efficiency issues:\n{{read_diff_output}}".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "compile_report".to_string(),
                description: "Final report".to_string(),
                depends_on: vec!["check_syntax".to_string(), "run_tests".to_string(), "quality_review".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Compile a code review report:\n- Syntax: {{check_syntax_output}}\n- Tests: {{run_tests_output}}\n- Quality: {{quality_review_output}}".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ],
        variables: HashMap::new(),
        timeout_secs: Some(300),
        default_retry_policy: oneai_workflow::RetryPolicy::default(),
        continue_on_failure: false,
    }
}

/// Debug workflow — systematic debugging: reproduce → search → analyze → fix → verify.
///
/// A 5-step DAG workflow that:
/// 1. Reproduces the bug
/// 2. Searches for error patterns in the code
/// 3. Analyzes root cause with LLM
/// 4. Applies fix (requires approval)
/// 5. Verifies the fix
fn debug_workflow() -> WorkflowConfig {
    WorkflowConfig {
        name: "debug".to_string(),
        description: "Systematic debugging: reproduce → search → analyze → fix → verify".to_string(),
        version: "1.0".to_string(),
        steps: vec![
            StepConfig {
                id: "reproduce".to_string(),
                description: "Reproduce the bug".to_string(),
                depends_on: vec![],
                tool: Some("shell".to_string()),
                tool_args: Some(serde_json::json!({"command": "{{reproduce_command}}"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(60),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "search_code".to_string(),
                description: "Search for error patterns".to_string(),
                depends_on: vec!["reproduce".to_string()],
                tool: Some("grep".to_string()),
                tool_args: Some(serde_json::json!({"pattern": "{{error_pattern}}", "path": "."})),
                prompt: None,
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "analyze".to_string(),
                description: "Root cause analysis".to_string(),
                depends_on: vec!["search_code".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Analyze this bug: error={{reproduce_output}}, related code={{search_code_output}}".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "fix".to_string(),
                description: "Apply fix".to_string(),
                depends_on: vec!["analyze".to_string()],
                tool: Some("shell".to_string()),
                tool_args: Some(serde_json::json!({"command": "{{fix_command}}"})),
                prompt: None,
                requires_approval: true, // Fix requires human approval
                timeout_secs: Some(60),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "verify".to_string(),
                description: "Verify fix".to_string(),
                depends_on: vec!["fix".to_string()],
                tool: Some("shell".to_string()),
                tool_args: Some(serde_json::json!({"command": "cargo test 2>&1 | tail -10"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(120),
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ],
        variables: HashMap::new(),
        timeout_secs: Some(300),
        default_retry_policy: oneai_workflow::RetryPolicy::default(),
        continue_on_failure: false,
    }
}

/// Refactor workflow — systematic refactoring: analyze → plan → execute → verify.
///
/// A 4-step DAG workflow that:
/// 1. Analyzes the current code structure and identifies refactoring targets
/// 2. Plans the refactoring approach (what to change, how to change it)
/// 3. Executes the refactoring changes (requires approval)
/// 4. Verifies that the refactoring didn't break anything
fn refactor_workflow() -> WorkflowConfig {
    WorkflowConfig {
        name: "refactor".to_string(),
        description: "Systematic refactoring: analyze → plan → execute → verify".to_string(),
        version: "1.0".to_string(),
        steps: vec![
            StepConfig {
                id: "analyze_code".to_string(),
                description: "Analyze current code structure".to_string(),
                depends_on: vec![],
                tool: None,
                tool_args: None,
                prompt: Some("Analyze the following code and identify refactoring opportunities. Focus on: duplicated logic, overly complex functions, poor naming, missing abstractions, and violations of SOLID principles.\n\nLook at the project structure and key files to understand the current design.".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "plan_refactor".to_string(),
                description: "Plan refactoring approach".to_string(),
                depends_on: vec!["analyze_code".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Based on the analysis: {{analyze_code_output}}\n\nCreate a detailed refactoring plan. For each change, specify:\n1. What file/function to modify\n2. The exact change to make\n3. Why this change improves the code\n4. Potential risks and how to mitigate them\n\nPrioritize changes by impact (high → low) and risk (low → high).".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "execute_refactor".to_string(),
                description: "Execute refactoring changes".to_string(),
                depends_on: vec!["plan_refactor".to_string()],
                tool: Some("shell".to_string()),
                tool_args: Some(serde_json::json!({"command": "{{refactor_command}}"})),
                prompt: None,
                requires_approval: true, // Refactoring changes require human approval
                timeout_secs: Some(120),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "verify_refactor".to_string(),
                description: "Verify refactoring didn't break anything".to_string(),
                depends_on: vec!["execute_refactor".to_string()],
                tool: Some("shell".to_string()),
                tool_args: Some(serde_json::json!({"command": "cargo check 2>&1 && cargo test 2>&1 | tail -20"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(120),
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ],
        variables: HashMap::new(),
        timeout_secs: Some(300),
        default_retry_policy: oneai_workflow::RetryPolicy::default(),
        continue_on_failure: false,
    }
}

/// Test workflow — systematic test generation: understand → generate → run → fix.
///
/// A 4-step DAG workflow that:
/// 1. Understands the code to be tested
/// 2. Generates test cases for the code
/// 3. Runs the generated tests
/// 4. Fixes any failing tests
fn test_workflow() -> WorkflowConfig {
    WorkflowConfig {
        name: "test".to_string(),
        description: "Systematic test generation: understand → generate → run → fix".to_string(),
        version: "1.0".to_string(),
        steps: vec![
            StepConfig {
                id: "understand_code".to_string(),
                description: "Understand the code to be tested".to_string(),
                depends_on: vec![],
                tool: None,
                tool_args: None,
                prompt: Some("Analyze the codebase to understand what needs testing. Identify:\n1. Public functions and their expected behavior\n2. Edge cases and error conditions\n3. Integration points between modules\n4. Current test coverage gaps\n\nFocus on the most critical and under-tested components.".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "generate_tests".to_string(),
                description: "Generate test cases".to_string(),
                depends_on: vec!["understand_code".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Based on the analysis: {{understand_code_output}}\n\nGenerate comprehensive test cases. For each function/module identified:\n1. Write unit tests for normal behavior\n2. Write edge case tests\n3. Write error condition tests\n4. Write integration tests where applicable\n\nUse the project's existing test framework (cargo test for Rust, pytest for Python, etc.). Output the complete test code.".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "run_tests".to_string(),
                description: "Run the generated tests".to_string(),
                depends_on: vec!["generate_tests".to_string()],
                tool: Some("shell".to_string()),
                tool_args: Some(serde_json::json!({"command": "cargo test 2>&1 | tail -30"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(120),
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "fix_failures".to_string(),
                description: "Fix failing tests".to_string(),
                depends_on: vec!["run_tests".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("The test run produced these results: {{run_tests_output}}\n\nAnalyze any test failures and fix them. Common issues:\n1. Test assumptions don't match actual behavior → adjust test expectations\n2. Missing imports or setup → add the required scaffolding\n3. Async/timing issues → add proper waits or mocks\n4. Environment dependencies → mock or isolate them\n\nProvide the corrected test code.".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ],
        variables: HashMap::new(),
        timeout_secs: Some(300),
        default_retry_policy: oneai_workflow::RetryPolicy::default(),
        continue_on_failure: true, // Tests can partially fail — still useful to see all results
    }
}

/// ReAct StateGraph — cyclic think → act → observe → think/end loop.
///
/// This is the core iterative pattern for coding tasks:
/// 1. think: LLM inference — model decides what to do
/// 2. act: Execute a tool call (if model requests one)
/// 3. approve: Optional human approval checkpoint
/// 4. end: Final answer (if model produces a direct answer)
fn react_state_graph() -> StateGraph {
    let mut graph = StateGraph::new("react-loop", "think");

    // Think node — LLM decides what to do
    graph.add_node(GraphNode {
        id: "think".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: None,
            use_streaming: true,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Act node — execute a tool call
    graph.add_node(GraphNode {
        id: "act".to_string(),
        action: NodeAction::ToolCall {
            tool_name: "{{selected_tool}}".to_string(),
            args_template: Some("{{tool_args}}".to_string()),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Approve node — human approval checkpoint (interrupt point)
    graph.add_node(GraphNode {
        id: "approve".to_string(),
        action: NodeAction::HumanApproval {
            description: "Approve tool execution before proceeding".to_string(),
        },
        interrupt: true,
        metadata: HashMap::new(),
    });

    // End node — produce final answer
    graph.add_node(GraphNode {
        id: "end".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some("Provide a final answer to the user's question.".to_string()),
            use_streaming: true,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // Edges: think → act (HasToolCalls), think → end (IsFinalAnswer)
    graph.add_edge(GraphEdge {
        from: "think".to_string(),
        to: "act".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "think".to_string(),
        to: "end".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });

    // act → approve → think (ReAct loop)
    graph.add_edge(GraphEdge {
        from: "act".to_string(),
        to: "approve".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "approve".to_string(),
        to: "think".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });

    graph.add_terminal("end".to_string());

    graph
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coding_pack_creation() {
        let pack = coding_pack("/tmp/test_project");

        assert_eq!(pack.name, "coding");
        assert_eq!(pack.tools.len(), 10); // 9 original + ApplyPatchTool
        assert_eq!(pack.tool_decorators.len(), 9); // 8 original + apply_patch
        assert_eq!(pack.context_sources.len(), 7); // 6 original + RepoMapSource
        assert!(!pack.system_prompt_template.is_empty());
    }

    #[test]
    fn test_coding_pack_permission_profile() {
        let pack = coding_pack("/tmp/test");

        // Auto-approve: read-only operations
        assert!(pack.permission_profile.auto_approve.contains("read_file"));
        assert!(pack.permission_profile.auto_approve.contains("grep"));
        assert!(pack.permission_profile.auto_approve.contains("glob"));

        // Require confirmation: state-modifying operations
        assert!(pack.permission_profile.require_confirmation.contains("edit_file"));
        assert!(pack.permission_profile.require_confirmation.contains("shell"));

        // Deny by default: dangerous commands
        assert!(!pack.permission_profile.deny_by_default.is_empty());
    }

    #[test]
    fn test_coding_pack_paradigm_strategies() {
        let pack = coding_pack("/tmp/test");

        assert!(pack.paradigm_strategies.len() >= 4);

        // Refactoring strategy
        let refactor = pack.paradigm_strategies.iter()
            .find(|s| s.trigger_pattern.contains("refactor"))
            .unwrap();
        assert_eq!(refactor.paradigm_sequence.len(), 3);

        // Search strategy
        let search = pack.paradigm_strategies.iter()
            .find(|s| s.trigger_pattern.contains("find"))
            .unwrap();
        assert_eq!(search.paradigm_sequence.len(), 1);
    }

    #[test]
    fn test_coding_pack_compression_template() {
        let pack = coding_pack("/tmp/test");

        assert_eq!(pack.compression_template.name, "coding");
        assert!(pack.compression_template.preserve_fields.contains(&"critical_files".to_string()));
        assert!(pack.compression_template.preserve_fields.contains(&"progress_status".to_string()));
        assert!(pack.compression_template.truncate_rules.contains_key("tool_output"));
        assert!(pack.compression_template.truncate_rules.contains_key("file_content"));
    }

    #[test]
    fn test_coding_pack_strategy_matching() {
        let pack = coding_pack("/tmp/test");

        // Should match refactoring tasks
        let refactor_match = pack.paradigm_strategies.iter()
            .find(|s| s.matches("Please refactor the auth module"));
        assert!(refactor_match.is_some());

        // Should match search tasks
        let search_match = pack.paradigm_strategies.iter()
            .find(|s| s.matches("Find all uses of authenticate"));
        assert!(search_match.is_some());

        // Should match bug tasks
        let bug_match = pack.paradigm_strategies.iter()
            .find(|s| s.matches("Fix the crash in login handler"));
        assert!(bug_match.is_some());
    }
}
