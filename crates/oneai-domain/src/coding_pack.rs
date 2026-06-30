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
    FileListTool, NotebookEditTool, EnvironmentTool, WebFetchTool, WebSearchTool,
    ApplyPatchTool,
};

use oneai_workflow::{WorkflowConfig, StepConfig, StateGraph, GraphNode, GraphEdge, NodeAction, EdgeCondition};

use crate::domain_pack::DomainPack;
use crate::tool_decorator::ToolDecorator;
use crate::permission_profile::{PermissionProfile, DenyPattern};
use crate::paradigm_strategy::{ParadigmStrategy, SubAgentTypeDefinition, SubAgentMergeStrategy, DomainParadigmKind};
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

**Tool Selection Rules (CRITICAL — always follow these):**

Always prefer the most specific tool available over shell commands:
- For reading files: use read_file (NOT shell cat/head/tail/less)
- For editing files: use edit_file (NOT shell sed/awk/perl -i)
- For creating files: use apply_patch or edit_file (NOT shell echo/tee)
- For listing directories: use list_directory (NOT shell ls/find -type d)
- For searching content: use grep (NOT shell grep/find)
- For finding files: use glob (NOT shell find/locate)

Use shell ONLY for: compilation, testing, git operations, package management, \
running scripts, and commands that have no dedicated tool equivalent. \
Shell is the LEAST preferred tool — always check if a specialized tool exists first.

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
delegate it to a specialized sub-agent or switch to a planning paradigm.

**Current information**: Your knowledge has a training cutoff. For anything that may \
have changed since then (recent news, latest library/framework versions, current \
prices, live data, recent documentation), call `web_search` to find current sources \
and `web_fetch` to read them — do not answer from memory. The current date/time is \
appended to this prompt; use it to judge recency.";

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
            budget: 40_000,
            modifies_files: false,
            merge_strategy: SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
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
            budget: 80_000,
            modifies_files: true,
            merge_strategy: SubAgentMergeStrategy::Merge,
            structured_output: None,
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
            budget: 30_000,
            modifies_files: false,
            merge_strategy: SubAgentMergeStrategy::PreserveOnly,
            structured_output: None,
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
            Arc::new(WebSearchTool::new()) as Arc<dyn Tool>,
        ],

        // Layer 1 supplement: Tool decorators — coding-specific descriptions
        // Each description follows the Anthropic/OpenAI recommended pattern:
        // - Positive: what the tool does and when to use it
        // - Negative: when NOT to use it, with explicit redirect to alternative
        // - Preference: "RECOMMENDED" language signals the model to prefer this tool
        tool_decorators: vec![
            ToolDecorator::with_description(
                "read_file",
                "Read source code files. Supports offset+limit for large files \
                to avoid overflowing the context window. Returns content with line numbers. \
                For binary files, returns base64-encoded data.\n\n\
                **RECOMMENDED** — always prefer read_file over shell for reading files:\n\
                - Do NOT use shell cat → use read_file\n\
                - Do NOT use shell head/tail → use read_file with offset+limit\n\
                - Do NOT use shell less → use read_file\n\
                - read_file returns structured line-numbered output, handles encoding, \
                and respects the context window budget"
            ),
            ToolDecorator::with_description(
                "edit_file",
                "Perform precise code edits using exact string matching. The old_string \
                must be unique in the file. This is a safe, targeted editing mechanism — \
                avoid large rewrites when targeted edits suffice.\n\n\
                **RECOMMENDED** — always prefer edit_file over shell for editing files:\n\
                - Do NOT use shell sed → use edit_file\n\
                - Do NOT use shell awk → use edit_file\n\
                - Do NOT use shell perl -i → use edit_file\n\
                - edit_file ensures exact, atomic replacements with validation, \
                unlike shell text manipulation which is error-prone and irreversible"
            ),
            ToolDecorator::with_description(
                "apply_patch",
                "Apply a unified diff patch to modify multiple files at once. Use for \
                multi-file refactoring, batch edits, and applying review suggestions. \
                The patch should be in standard unified diff format. Each file's changes \
                are applied atomically — context mismatches skip that file with an error.\n\n\
                **RECOMMENDED** for multi-file changes — prefer apply_patch over shell patch command"
            ),
            ToolDecorator::with_description_and_permission(
                "shell",
                "Execute shell commands. ONLY use shell for operations that have NO \
                dedicated tool equivalent:\n\
                - Compilation: cargo build, npm run build, make\n\
                - Testing: cargo test, npm test, pytest\n\
                - Git: git status, git diff, git log, git commit\n\
                - Package management: cargo add, npm install, pip install\n\
                - Running scripts: python script.py, bash script.sh\n\n\
                **Do NOT use shell for file operations — use dedicated tools instead**:\n\
                - Do NOT use cat → use read_file\n\
                - Do NOT use sed/awk → use edit_file\n\
                - Do NOT use echo > file → use edit_file or apply_patch\n\
                - Do NOT use ls → use list_directory\n\
                - Do NOT use grep → use grep tool\n\
                - Do NOT use find → use glob\n\
                - Do NOT use mkdir → use edit_file (creates files), list_directory checks dirs\n\n\
                Commands run with timeout (default 120s, max 600s). Dangerous commands \
                (rm -rf, mkfs, dd, chmod 777) are blocked. Output truncated at 100KB.",
                PermissionLevel::Full
            ),
            ToolDecorator::with_description(
                "grep",
                "Search code content using regex patterns. Recursively searches the project \
                directory for matching lines with file paths and line numbers. Use for: finding \
                function definitions, usages, patterns across the codebase.\n\n\
                **RECOMMENDED** — prefer this over shell grep:\n\
                - Do NOT use shell grep → use this grep tool\n\
                - Do NOT use shell rg/ag → use this grep tool\n\
                - Native Rust implementation, no shell dependency, respects context limits"
            ),
            ToolDecorator::with_description(
                "glob",
                "Find files matching glob patterns (e.g., '**/*.rs', 'src/**/*.toml'). \
                Faster than grep for file discovery. Use for: finding source files, config \
                files, test files.\n\n\
                **RECOMMENDED** — prefer this over shell find:\n\
                - Do NOT use shell find → use glob\n\
                - Do NOT use shell locate → use glob\n\
                - Native Rust implementation, no shell dependency"
            ),
            ToolDecorator::with_description(
                "list_directory",
                "List directory contents showing files and subdirectories with sizes. \
                Use for: exploring project structure, finding relevant directories.\n\n\
                **RECOMMENDED** — prefer this over shell ls:\n\
                - Do NOT use shell ls → use list_directory\n\
                - Do NOT use shell find -type d → use list_directory\n\
                - Returns structured output with file sizes, unlike raw ls output"
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
                fetching documentation, API references, blog posts, and any web content.\n\n\
                **RECOMMENDED** — prefer this over shell curl for URL content:\n\
                - Do NOT use shell curl → use web_fetch (returns structured content)\n\
                - Only use shell curl when you need raw binary data or streaming"
            ),
            ToolDecorator::with_description(
                "web_search",
                "Search the web for current information. Returns titles, URLs, and \
                snippets. Use this whenever a question is time-sensitive or may have \
                changed since your training (recent news, latest releases/versions, \
                current prices, live data, recent docs), then use web_fetch to read the \
                most promising results.\n\n\
                **RECOMMENDED** for fresh information:\n\
                - For 'latest' / 'current' / 'recent' / 'new' questions → web_search first\n\
                - Do NOT guess from memory for anything that may have changed\n\
                - Combine web_search + web_fetch; cross-reference multiple results"
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
                "web_search".to_string(),
                "web_fetch".to_string(),
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

        // Layer 7: Memory profile — coding memory policy
        memory_profile: crate::memory_profile::MemoryProfile::coding(),

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
            plan_workflow_state_graph(),
            reflect_workflow_state_graph(),
            explore_workflow_state_graph(),
        ],
        sub_agent_definitions: SubAgentTypeDefinition::defaults(),
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
            include_tool_definitions: true,  // P2-2: Send tools so model can decide to call them
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
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
            include_tool_definitions: false,  // P2-2: No tools for final answer node
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
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

/// Plan-workflow graph — structured task decomposition.
///
/// A single `plan` LlmInfer node loops until the model produces a final
/// answer (the plan). Tool definitions are restricted to the plan control
/// tools (`exit_plan_mode`, `task_create`) so the model is nudged toward
/// committing a plan rather than executing real tools. If the model delegates
/// a sub-planning task, a `delegate` node spawns a Plan sub-agent and returns
/// to `plan`.
///
/// Termination: every `plan` iteration either reaches `end` (IsFinalAnswer,
/// terminal) or loops back — bounded by `StateGraphExecutor::max_iterations`.
fn plan_workflow_state_graph() -> StateGraph {
    let mut graph = StateGraph::new("plan-workflow", "plan");

    // plan node — decompose the task; only plan control tools are exposed.
    graph.add_node(GraphNode {
        id: "plan".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "You are a planning agent. Decompose the task into ordered, \
                dependency-ordered steps. Use `task_create` to commit the step \
                list, then `exit_plan_mode` to submit, or give the plan as \
                your final answer text.".to_string(),
            ),
            use_streaming: true,
            include_tool_definitions: true,
            tool_filter_override: Some(vec![
                "exit_plan_mode".to_string(),
                "task_create".to_string(),
            ]),
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // delegate node — hand a sub-planning task to a Plan sub-agent.
    graph.add_node(GraphNode {
        id: "delegate".to_string(),
        action: NodeAction::Delegate {
            agent_kind: "Plan".to_string(),
            task_template: "{{task}}".to_string(),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // end node — final plan answer.
    graph.add_node(GraphNode {
        id: "end".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "Present the finalized plan to the user as a clear, ordered \
                list of steps.".to_string(),
            ),
            use_streaming: true,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // plan → end (final answer), plan → delegate (sub-planning), plan → plan (tool calls loop)
    graph.add_edge(GraphEdge {
        from: "plan".to_string(),
        to: "end".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "plan".to_string(),
        to: "delegate".to_string(),
        condition: Some(EdgeCondition::RequestsDelegation),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "plan".to_string(),
        to: "plan".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "delegate".to_string(),
        to: "plan".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });

    graph.add_terminal("end".to_string());
    graph
}

/// Reflect-workflow graph — critical review of the last result.
///
/// A `reflect` LlmInfer node reasons about `last_result` and either produces
/// a final assessment (`end`) or calls a tool to verify/fix (`act`), looping
/// back. Mirrors the ReAct shape but with a reflection-focused system prompt
/// so the node's job is review, not open-ended action.
///
/// Termination: reflect → end (IsFinalAnswer, terminal) or reflect → act →
/// reflect, bounded by `max_iterations`.
fn reflect_workflow_state_graph() -> StateGraph {
    let mut graph = StateGraph::new("reflect-workflow", "reflect");

    graph.add_node(GraphNode {
        id: "reflect".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "You are a reflection agent. Critically review the last result: \
                check for correctness, gaps, and regressions. If a verification \
                or fix is needed, call the right tool; otherwise give your \
                final assessment as text.".to_string(),
            ),
            use_streaming: true,
            include_tool_definitions: true,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_node(GraphNode {
        id: "act".to_string(),
        action: NodeAction::ToolCall {
            tool_name: "{{selected_tool}}".to_string(),
            args_template: Some("{{tool_args}}".to_string()),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_node(GraphNode {
        id: "end".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "Provide the final reflection assessment.".to_string(),
            ),
            use_streaming: true,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_edge(GraphEdge {
        from: "reflect".to_string(),
        to: "end".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "reflect".to_string(),
        to: "act".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "act".to_string(),
        to: "reflect".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });

    graph.add_terminal("end".to_string());
    graph
}

/// Explore-workflow graph — breadth-first search with delegation.
///
/// An `explore` LlmInfer node searches/understands the environment. It can
/// call real tools (`act`), delegate sub-explorations to an Explore sub-agent
/// (`delegate`), or finish by producing findings that the `synthesize` node
/// rolls into a final summary.
///
/// Termination: explore → synthesize (IsFinalAnswer, terminal); other paths
/// loop back, bounded by `max_iterations`.
fn explore_workflow_state_graph() -> StateGraph {
    let mut graph = StateGraph::new("explore-workflow", "explore");

    graph.add_node(GraphNode {
        id: "explore".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "You are an exploration agent. Search and understand the \
                codebase/environment. Call tools to inspect, or delegate \
                focused sub-searches to an Explore sub-agent. When you have \
                enough, give your findings as your final answer.".to_string(),
            ),
            use_streaming: true,
            include_tool_definitions: true,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_node(GraphNode {
        id: "act".to_string(),
        action: NodeAction::ToolCall {
            tool_name: "{{selected_tool}}".to_string(),
            args_template: Some("{{tool_args}}".to_string()),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_node(GraphNode {
        id: "delegate".to_string(),
        action: NodeAction::Delegate {
            agent_kind: "Explore".to_string(),
            task_template: "{{task}}".to_string(),
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    graph.add_node(GraphNode {
        id: "synthesize".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some(
                "Synthesize the exploration findings into a comprehensive \
                final summary: file paths, key signatures, and patterns.".to_string(),
            ),
            use_streaming: true,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });

    // explore → synthesize (final answer), explore → delegate (sub-search),
    // explore → act (real tool), both loop back to explore.
    graph.add_edge(GraphEdge {
        from: "explore".to_string(),
        to: "synthesize".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "explore".to_string(),
        to: "delegate".to_string(),
        condition: Some(EdgeCondition::RequestsDelegation),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "explore".to_string(),
        to: "act".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "act".to_string(),
        to: "explore".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });
    graph.add_edge(GraphEdge {
        from: "delegate".to_string(),
        to: "explore".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });

    graph.add_terminal("synthesize".to_string());
    graph
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coding_pack_creation() {
        let pack = coding_pack("/tmp/test_project");

        assert_eq!(pack.name, "coding");
        assert_eq!(pack.tools.len(), 11); // 9 original + ApplyPatchTool + WebSearchTool
        assert_eq!(pack.tool_decorators.len(), 10); // 8 original + apply_patch + web_search
        assert_eq!(pack.context_sources.len(), 7); // 6 original + RepoMapSource
        assert!(!pack.system_prompt_template.is_empty());
    }

    #[test]
    fn test_coding_pack_state_graphs() {
        let pack = coding_pack("/tmp/test_project");

        // All four paradigm graphs must be registered.
        assert_eq!(pack.state_graphs.len(), 4);
        let names: Vec<&str> = pack.state_graphs.iter()
            .map(|g| g.name.as_str()).collect();
        for expected in ["react-loop", "plan-workflow", "reflect-workflow", "explore-workflow"] {
            assert!(
                names.contains(&expected),
                "missing state graph '{}'; got: {:?}", expected, names
            );
        }

        // Each graph must have an entry point and at least one terminal node
        // (guaranteed termination path for the StateGraphExecutor).
        for g in &pack.state_graphs {
            assert!(!g.entry_point.is_empty(), "graph '{}' has no entry point", g.name);
            assert!(!g.terminal_nodes.is_empty(), "graph '{}' has no terminal nodes", g.name);
            assert!(g.node_count() > 0, "graph '{}' has no nodes", g.name);
        }

        // Spot-check the new graphs' terminals.
        let terminal_of = |name: &str| -> &Vec<String> {
            pack.state_graphs.iter()
                .find(|g| g.name == name)
                .expect("graph must exist")
                .terminal_nodes.as_ref()
        };
        assert_eq!(terminal_of("plan-workflow"), &vec!["end".to_string()]);
        assert_eq!(terminal_of("reflect-workflow"), &vec!["end".to_string()]);
        assert_eq!(terminal_of("explore-workflow"), &vec!["synthesize".to_string()]);
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
