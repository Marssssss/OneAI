//! OneAI CLI — interactive REPL and non-interactive inference.
//!
//! Subcommands:
//!   oneai chat          — Launch the interactive TUI
//!   oneai run <prompt>  — Single-shot inference (stdout)
//!   oneai studio        — Launch Studio Web UI (port 3000)
//!   oneai embed generate <text> — Generate embedding for text
//!   oneai embed batch <texts>   — Generate embeddings for comma-separated texts
//!   oneai embed list              — List available embedding models
//!   oneai embed health            — Check embedding service health
//!   oneai embed dimension         — Show embedding dimension
//!   oneai pack list     — List available DomainPacks
//!   oneai pack show <n> — Show DomainPack details
//!   oneai pack install  — Install a DomainPack
//!   oneai pack validate — Validate a DomainPack spec file
//!   oneai pack spec     — Export DomainPack spec as JSON Schema
//!   oneai pack check <n>— Check installed pack against spec
//!   oneai mcp serve     — Run as MCP server (Stdio mode)
//!   oneai mcp list      — List configured MCP servers
//!   oneai mcp add <n>   — Add MCP server config
//!   oneai mcp remove <n>— Remove MCP server config
//!   oneai mcp connect <n>— Test MCP server connection
//!   oneai a2a serve       — Start A2A server
//!   oneai a2a discover <url> — Discover remote A2A agent
//!   oneai a2a list        — List configured A2A endpoints
//!   oneai a2a send <url> <msg> — Send task to remote agent
//!   oneai wasm list       — List loaded WASM modules
//!   oneai wasm load <n> <f> — Load a WASM module
//!   oneai wasm run <n>   — Execute a WASM module
//!   oneai wasm health    — Check WASM module health
//!   oneai wasm unload <n>— Unload a WASM module
//!   oneai wasm stats     — Show resource monitor statistics
//!   oneai session list   — List all saved sessions
//!   oneai session resume <id> — Resume a saved session
//!   oneai session delete <id> — Delete a session
//!   oneai session info <id>   — Show session details
//!   oneai usage report          — Show global usage summary
//!   oneai usage session <id>    — Show per-session usage details
//!   oneai usage export [--format]— Export usage records (json/csv)
//!   oneai provider status      — Show provider pool status and health
//!   oneai provider fallback-log — Show recent fallback events
//!   oneai provider test        — Test all providers connectivity
//!   oneai eval list     — List available eval suites
//!   oneai eval run <n>  — Run an eval suite
//!   oneai eval score <n>— Run metrics only (no agent)
//!   oneai config show   — Show current configuration
//!   oneai config init   — Create default config file
//!   oneai version       — Version information
//!   oneai init [--format oneai|agents|claude] [--path <dir>] [--force] [--no-llm]
//!                      — Generate project-instruction file (ONEAI.md/AGENTS.md/CLAUDE.md)
//!   oneai handoff list  — List available handoff targets
//!   oneai handoff targets <p> — Show handoff target descriptions
//!   oneai handoff config [<p>] — Show handoff configuration
//!   oneai handoff run <t> <r> — Execute a handoff
//!   oneai swarm list   — List available swarm presets
//!   oneai swarm routing — Show routing strategies
//!   oneai swarm config <p> — Show swarm configuration
//!   oneai swarm agents <p> — Show swarm agent capabilities
//!   oneai swarm run <task> — Execute a swarm task
//!   oneai workflow list  — List DAG workflows + state graphs in the active pack
//!   oneai workflow show <n> — Render a workflow DAG as ASCII
//!   oneai workflow run <n> [task] — Execute a DAG workflow with a real LLM
//!   oneai graph list     — List state graphs
//!   oneai graph show <n> — Render a state graph as ASCII
//!   oneai graph run <n> <task> — Execute a state graph with a real LLM

mod config;
mod cmd_chat;
mod cmd_run;
mod cmd_tasks;
mod cmd_pack;
mod cmd_skill;
mod cmd_eval;
mod cmd_config;
mod cmd_init;
mod cmd_version;
mod cmd_studio;
mod cmd_mcp;
mod cmd_a2a;
mod cmd_wasm;
mod cmd_session;
mod cmd_memory;
mod cmd_embed;
mod cmd_usage;
mod cmd_provider;
mod cmd_token;
mod cmd_team;
mod cmd_handoff;
mod cmd_swarm;
mod cmd_workflow;
mod tui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "oneai",
    version,
    about = "OneAI — Rust Agent Framework CLI",
    long_about = "OneAI is a Rust Agent framework with pluggable domain configuration (DomainPack), \
                  dynamic paradigm switching, and WASM sandbox execution.\n\n\
                  Use 'oneai chat' for interactive mode or 'oneai run <prompt>' for non-interactive inference."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch the interactive TUI (default when no subcommand given)
    Chat {
        /// Domain pack to use (coding, research, general)
        #[arg(long)]
        domain: Option<String>,
        /// Model to use (overrides config and env)
        #[arg(long)]
        model: Option<String>,
        /// User id — namespaces cross-session memory/habits ("越用越好用")
        #[arg(long)]
        user: Option<String>,
    },
    /// Run a single-shot inference and output to stdout
    Run {
        /// The prompt to send to the agent
        prompt: String,
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
        /// Model to use
        #[arg(long)]
        model: Option<String>,
        /// User id — namespaces cross-session memory/habits
        #[arg(long)]
        user: Option<String>,
    },
    /// Launch Studio Web UI for visualizing agent execution
    Studio {
        /// Port to listen on (default: 3000)
        #[arg(long, default_value_t = 3000)]
        port: u16,
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
        /// Model name override (overrides ONEAI_MODEL / config)
        #[arg(long)]
        model: Option<String>,
        /// User identity for memory namespacing
        #[arg(long)]
        user: Option<String>,
    },
    /// Manage domain packs
    Pack {
        #[command(subcommand)]
        action: PackAction,
    },
    /// Manage skills — list/show skills discovered from convention directories
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Run evaluation suites
    Eval {
        #[command(subcommand)]
        action: EvalAction,
    },
    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Manage MCP server plugins and run as MCP server
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// A2A agent-to-agent protocol
    A2a {
        #[command(subcommand)]
        action: A2aAction,
    },
    /// Manage WASM modules and sandbox execution
    Wasm {
        #[command(subcommand)]
        action: WasmAction,
    },
    /// Manage saved sessions (requires SQLite persistence)
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Manage durable working state — cross-session task continuation
    /// (list/show/continue/archive unfinished tasks; the durable source is
    /// independent of any session transcript)
    Tasks {
        #[command(subcommand)]
        action: TasksAction,
    },
    /// Manage long-term memory — search/list durable facts (cross-session habits)
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Embedding service — generate vector embeddings for text
    Embed {
        #[command(subcommand)]
        action: EmbedAction,
    },
    /// Usage management — track LLM inference token usage (prompt/completion/total/calls)
    Usage {
        #[command(subcommand)]
        action: UsageAction,
    },
    /// Provider pool management — multi-provider fallback status and health
    Provider {
        #[command(subcommand)]
        action: ProviderAction,
    },
    /// Token counting & context management — count tokens, context windows, fit checks
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Team coordination — multi-agent team strategies and execution
    Team {
        #[command(subcommand)]
        action: TeamAction,
    },
    /// Handoff protocol — agent handoff-as-tool-call targets and configuration
    Handoff {
        #[command(subcommand)]
        action: HandoffAction,
    },
    /// Swarm orchestration — dynamic agent pools with capability-driven routing
    Swarm {
        #[command(subcommand)]
        action: SwarmAction,
    },
    /// DAG workflows and cyclic StateGraphs — list/show/run the predefined
    /// workflows and state graphs embedded in the active DomainPack
    /// (e.g. CodingPack's code_review/debug/refactor/test workflows + the
    /// react/plan/reflect/explore state graphs).
    Workflow {
        #[command(subcommand)]
        action: WorkflowAction,
    },
    /// State graph commands — list/show/run cyclic StateGraphs
    Graph {
        #[command(subcommand)]
        action: GraphAction,
    },
    /// Show version information
    Version,
    /// Generate a project-instruction file (ONEAI.md / AGENTS.md / CLAUDE.md)
    ///
    /// Analyzes the current project heuristically (build system, commands,
    /// structure, dependencies, conventions, git context) and writes a markdown
    /// file that is auto-loaded into agent context. Mirrors Claude Code's /init
    /// and OpenCode's /init.
    Init {
        /// Output format: oneai (ONEAI.md), agents (AGENTS.md), claude (CLAUDE.md)
        #[arg(long, default_value = "oneai")]
        format: String,
        /// Target project directory (default: current directory)
        #[arg(long)]
        path: Option<String>,
        /// Overwrite an existing instruction file
        #[arg(long)]
        force: bool,
        /// Skip LLM synthesis; write a deterministic heuristic doc instead
        #[arg(long)]
        no_llm: bool,
    },
}

#[derive(Subcommand)]
enum PackAction {
    /// List available domain packs
    List,
    /// Show details of a domain pack
    Show {
        /// Pack name
        name: String,
    },
    /// Install a domain pack from a path or git URL
    Install {
        /// Source path or git URL
        source: String,
    },
    /// Validate a DomainPack spec file (structural + semantic checks)
    Validate {
        /// Path to the DomainPack config file (.yaml, .yml, or .toml)
        path: String,
    },
    /// Export DomainPack specification as JSON Schema
    Spec,
    /// Check an installed pack against the specification
    Check {
        /// Pack name to check
        name: String,
    },
}

#[derive(Subcommand)]
enum SkillAction {
    /// List skills discovered from convention directories
    /// (.claude/skills · .agents/skills · .opencode/skills · .oneai/skills)
    List,
    /// Show details of a discovered skill
    Show {
        /// Skill name
        name: String,
    },
}

#[derive(Subcommand)]
enum EvalAction {
    /// List available eval suites
    List,
    /// Run an eval suite with agent execution
    Run {
        /// Suite name (coding_basics, tool_use, general)
        name: String,
        /// Output format (markdown, json, compact)
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Emit the efficiency axis: per-case inference/tool/overhead
        /// wall-clock, tokens, iterations, cache hit ratio + three-axis
        /// score (quality×tokens×latency).
        #[arg(long)]
        profile: bool,
        /// Record the first case's trajectory (provider responses + tool-call
        /// sequence + iteration count) to <path> as JSON, for later
        /// `oneai eval replay <path>` determinism checks.
        #[arg(long)]
        record: Option<String>,
    },
    /// Run metrics only (no agent execution — uses expected answers as outputs)
    Score {
        /// Suite name
        name: String,
    },
    /// Replay a recorded trajectory (ghost replay) — re-runs the agent with a
    /// frozen provider (no live LLM) and checks determinism: same tool calls
    /// in the same order, within the recorded iteration count. The loop-test
    /// primitive from Loop Engineering.
    Replay {
        /// Path to a recorded trajectory JSON file.
        path: String,
    },
    /// Run SWE-bench instances (能力×成本×效率 three-axis eval).
    ///
    /// Clones each instance's repo at base_commit, drives the agent on the
    /// problem statement, captures `git diff` as the patch, and judges it via
    /// the external SWE-bench harness (Python subprocess).
    Swebench {
        /// Path to a SWE-bench JSONL dataset (instance rows).
        #[arg(long)]
        dataset: String,
        /// Comma-separated instance ids to run (default: all in the dataset).
        #[arg(long)]
        instances: Option<String>,
        /// Workspace dir for cloned repos + artifacts (default ./swebench-workspace).
        #[arg(long, default_value = "./swebench-workspace")]
        workspace: String,
        /// Python interpreter with `swebench` installed (default ~/.venvs/swebench/bin/python).
        #[arg(long)]
        python: Option<String>,
        /// Run the judge harness via Modal (default true; set false for local docker).
        #[arg(long, default_value_t = true)]
        modal: bool,
        /// Dataset name passed to the harness (e.g. princeton-nlp/SWE-bench_Lite).
        #[arg(long, default_value = "princeton-nlp/SWE-bench_Lite")]
        dataset_name: String,
        /// Cap on number of instances to run (0 = no cap).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        /// Output format (markdown, json, compact).
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Run id for the SWE-bench harness (results land in evaluation_results/<run_id>/).
        #[arg(long, default_value = "oneai")]
        run_id: String,
    },
    /// Run the memory-subsystem eval suite (LongMemEval 5-ability + Mem0
    /// F1/BLEU-1 + Recall@k/NDCG@k). Aligned with docs/memory-mechanism.md §14.
    /// The headline anchor: `--no-embedding` (keyword baseline) vs default
    /// (semantic) on the synonym anti-example quantifies the §12.1 gain.
    Memory {
        /// Suite source: `builtin` (synthetic 5-ability suite) or `jsonl`
        /// (load cases from --data, LoCoMo/LongMemEval-compatible JSONL).
        #[arg(long, default_value = "builtin")]
        suite: String,
        /// Path to a JSONL suite file (when --suite jsonl).
        #[arg(long)]
        data: Option<String>,
        /// Comma-separated metrics: recall_at_k,ndcg_at_k,f1,bleu1,abstention,judge.
        #[arg(long, default_value = "recall_at_k,f1,bleu1")]
        metrics: String,
        /// Disable semantic recall (keyword-only baseline — the §12.1 control).
        #[arg(long)]
        no_embedding: bool,
        /// k for Recall@k / NDCG@k.
        #[arg(long, default_value_t = 5)]
        k: usize,
        /// Output format (markdown, json, compact).
        #[arg(long, default_value = "markdown")]
        format: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration
    Show,
    /// Create default configuration file
    Init,
}

#[derive(Subcommand)]
enum McpAction {
    /// Run as MCP server (Stdio mode — for integration with Claude Code/Cursor)
    Serve {
        /// Domain pack to expose via MCP
        #[arg(long)]
        domain: Option<String>,
    },
    /// List configured MCP servers
    List,
    /// Add an MCP server configuration
    Add {
        /// Server name
        name: String,
        /// Transport type: stdio, sse, streamable_http
        #[arg(long)]
        transport: String,
        /// Command to launch (for stdio transport)
        #[arg(long)]
        command: Option<String>,
        /// URL endpoint (for sse/streamable_http transport)
        #[arg(long)]
        url: Option<String>,
        /// Command arguments (comma-separated, for stdio transport)
        #[arg(long)]
        args: Option<String>,
        /// Whether server is enabled
        #[arg(long, default_value_t = true)]
        enabled: bool,
    },
    /// Remove an MCP server configuration
    Remove {
        /// Server name
        name: String,
    },
    /// Test connecting to an MCP server and show discovered tools
    Connect {
        /// Server name
        name: String,
    },
}

#[derive(Subcommand)]
enum A2aAction {
    /// Start A2A server (serve OneAI agent capabilities)
    Serve {
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
    },
    /// Discover a remote A2A agent's capabilities
    Discover {
        /// Agent URL endpoint
        url: String,
    },
    /// List configured A2A endpoints
    List,
    /// Send a task to a remote A2A agent
    Send {
        /// Agent URL endpoint
        url: String,
        /// Task message
        message: String,
    },
}

#[derive(Subcommand)]
enum WasmAction {
    /// List loaded WASM modules
    List,
    /// Load a WASM module from file
    Load {
        /// Module name (identifier in registry)
        name: String,
        /// Path to .wasm file
        file: String,
    },
    /// Execute a loaded WASM module with JSON input
    Run {
        /// Module name
        name: String,
        /// JSON input string
        #[arg(long)]
        input: Option<String>,
        /// Input file path (alternative to --input)
        #[arg(long)]
        input_file: Option<String>,
    },
    /// Check WASM module health
    Health {
        /// Module name (optional — checks all if not specified)
        #[arg(long)]
        name: Option<String>,
    },
    /// Unload a WASM module
    Unload {
        /// Module name
        name: String,
    },
    /// Show resource monitor statistics
    Stats,
}

#[derive(Subcommand)]
enum SessionAction {
    /// List all saved sessions
    List,
    /// Resume a saved session (show conversation history)
    Resume {
        /// Session ID to resume
        id: String,
    },
    /// Delete a saved session
    Delete {
        /// Session ID to delete
        id: String,
    },
    /// Show detailed info about a session
    Info {
        /// Session ID to inspect
        id: String,
    },
}

#[derive(Subcommand)]
enum TasksAction {
    /// List open (unfinished) tasks for the current user/project
    List {
        /// User id (defaults to all)
        #[arg(long)]
        user: Option<String>,
        /// Working-state root (default: ./.oneai)
        #[arg(long)]
        root: Option<String>,
    },
    /// Show a task's goal / steps / decisions / blockers
    Show {
        /// Task id
        id: String,
        /// Working-state root (default: ./.oneai)
        #[arg(long)]
        root: Option<String>,
    },
    /// Start a NEW session bound to an existing unfinished task (cross-session
    /// continuation — does not read the old session's transcript)
    Continue {
        /// Task id to continue
        id: String,
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
        /// Model to use
        #[arg(long)]
        model: Option<String>,
        /// User id — namespaces cross-session memory
        #[arg(long)]
        user: Option<String>,
        /// Working-state root (default: ./.oneai)
        #[arg(long)]
        root: Option<String>,
    },
    /// Archive a task (mark done, gzip its event log)
    Archive {
        /// Task id to archive
        id: String,
        /// Working-state root (default: ./.oneai)
        #[arg(long)]
        root: Option<String>,
    },
}

#[derive(Subcommand)]
enum MemoryAction {
    /// Search durable facts by keyword
    Search {
        /// Keyword query
        query: String,
        /// User id whose facts to search (defaults to "default")
        #[arg(long, default_value = "default")]
        user: String,
        /// Max facts to return
        #[arg(long, default_value_t = 10)]
        top_k: usize,
    },
    /// List durable facts for a user (cross-session) and/or session
    List {
        #[arg(long, default_value = "default")]
        user: String,
        /// Scope to a session id (omit for all of the user's facts)
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Subcommand)]
enum EmbedAction {
    /// Generate an embedding for a text string
    Generate {
        /// Text to embed
        text: String,
        /// Embedding model to use
        #[arg(long)]
        model: Option<String>,
        /// Service type: fastembed, ollama, openai, anthropic
        #[arg(long)]
        service: Option<String>,
        /// API key (required for openai/anthropic services)
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Generate embeddings for multiple comma-separated texts
    Batch {
        /// Comma-separated texts to embed
        texts: String,
        /// Embedding model to use
        #[arg(long)]
        model: Option<String>,
        /// Service type: fastembed, ollama, openai, anthropic
        #[arg(long)]
        service: Option<String>,
        /// API key (required for openai/anthropic services)
        #[arg(long)]
        api_key: Option<String>,
    },
    /// List available embedding models
    List,
    /// Check embedding service health
    Health {
        /// Embedding model to use
        #[arg(long)]
        model: Option<String>,
        /// Service type: fastembed, ollama, openai, anthropic
        #[arg(long)]
        service: Option<String>,
        /// API key (required for openai/anthropic services)
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Show embedding dimension for a model
    Dimension {
        /// Embedding model to use
        #[arg(long)]
        model: Option<String>,
        /// Service type: fastembed, ollama, openai, anthropic
        #[arg(long)]
        service: Option<String>,
        /// API key (required for openai/anthropic services)
        #[arg(long)]
        api_key: Option<String>,
    },
}

#[derive(Subcommand)]
enum UsageAction {
    /// Show global usage summary (total tokens, calls, by-model breakdown)
    Report,
    /// Show per-session usage details
    Session {
        /// Session ID to inspect
        id: String,
    },
    /// Export usage records (json or csv format)
    Export {
        /// Export format: json or csv (default: json)
        #[arg(long, default_value = "json")]
        format: String,
    },
}

#[derive(Subcommand)]
enum ProviderAction {
    /// Show provider pool status — active provider, health, circuit states
    Status,
    /// Show recent fallback events from the pool log
    FallbackLog {
        /// Number of events to show (default: 20)
        #[arg(long, default_value = "20")]
        limit: String,
    },
    /// Test all providers in the pool with a connectivity check
    Test,
    /// Show routing decision for a task (dry run) — cost/latency/quality analysis
    Route {
        /// Task description to route
        task: String,
        /// Routing strategy (balanced, cost, latency, quality)
        #[arg(long, default_value = "balanced")]
        strategy: String,
    },
    /// Show recent routing decisions with rationale
    RouteLog {
        /// Number of decisions to show (default: 10)
        #[arg(long, default_value = "10")]
        limit: String,
    },
    /// Show current routing strategy and configuration
    RouteConfig,
}

#[derive(Subcommand)]
enum TokenAction {
    /// Count tokens in a text string
    Count {
        /// Text to count tokens for
        text: String,
        /// Model to use for estimation (affects chars-per-token ratio)
        #[arg(long)]
        model: Option<String>,
    },
    /// Estimate tokens in a sample conversation
    Estimate {
        /// Model to use for estimation
        #[arg(long)]
        model: Option<String>,
    },
    /// Show context window profile for a model
    Context {
        /// Model name to show profile for
        model: String,
    },
    /// List all known tokenizer profiles
    Models,
    /// Check if text fits within a model's context window
    Fits {
        /// Text to check fit for
        text: String,
        /// Model to check against
        #[arg(long)]
        model: String,
    },
    /// Probe the provider's model-metadata endpoint for the context window
    /// (L2), showing the full 3-layer resolution and which layer won.
    Probe {
        /// Model to probe (defaults to the configured model)
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum TeamAction {
    /// List available team coordination strategies
    Strategies,
    /// List preset team configurations (code_review, research_route, dev_pipeline, arch_debate)
    Presets,
    /// Show team configuration details
    Info {
        /// Team ID or preset name
        id: String,
    },
    /// Run a team coordination task
    Run {
        /// The task to coordinate
        task: String,
        /// Team strategy: coordinate, route, collaborate, debate
        #[arg(long, default_value = "coordinate")]
        strategy: String,
        /// Use a preset team configuration
        #[arg(long)]
        preset: Option<String>,
        /// Total token budget for the team (default: 100000)
        #[arg(long)]
        budget: Option<String>,
    },
}

#[derive(Subcommand)]
enum HandoffAction {
    /// List available handoff targets and presets
    List,
    /// Show detailed handoff target descriptions for a preset
    Targets {
        /// Preset name (development_chain, research_chain, support_routing)
        preset: String,
    },
    /// Show current handoff configuration
    Config {
        /// Preset name (optional — defaults to development_chain)
        #[arg(long)]
        preset: Option<String>,
    },
    /// Execute a handoff to a target agent (demo mode)
    Run {
        /// Target agent name
        target: String,
        /// Reason for handoff
        reason: String,
        /// Preset name (optional — defaults to development_chain)
        #[arg(long)]
        preset: Option<String>,
    },
}

#[derive(Subcommand)]
enum SwarmAction {
    /// List available swarm presets
    List,
    /// Show available routing strategies with descriptions
    Routing,
    /// Show swarm configuration details for a preset
    Config {
        /// Preset name (code_analysis, fast_research, balanced_dev)
        preset: String,
    },
    /// Show agents and capabilities in a swarm preset
    Agents {
        /// Preset name
        preset: String,
    },
    /// Execute a swarm task
    Run {
        /// The task to execute
        task: String,
        /// Routing strategy (best-fit, load-balanced, cost-optimized, fastest)
        #[arg(long, default_value = "best-fit")]
        routing: String,
        /// Use a preset swarm configuration
        #[arg(long)]
        preset: Option<String>,
        /// Total token budget for the swarm (default: 100000)
        #[arg(long)]
        budget: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkflowAction {
    /// List available DAG workflows and state graphs in the active domain pack
    List {
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
    },
    /// Render a workflow DAG as ASCII and list its steps
    Show {
        /// Workflow name
        name: String,
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
    },
    /// Execute a DAG workflow end-to-end with a real LLM provider
    Run {
        /// Workflow name
        name: String,
        /// Optional task input (some workflows read {{task}}; others are
        /// self-contained shell/prompt chains)
        task: Option<String>,
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
        /// Model to use
        #[arg(long)]
        model: Option<String>,
        /// User id — namespaces cross-session memory/habits
        #[arg(long)]
        user: Option<String>,
    },
}

#[derive(Subcommand)]
enum GraphAction {
    /// List available state graphs in the active domain pack
    List {
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
    },
    /// Render a state graph as ASCII
    Show {
        /// State graph name
        name: String,
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
    },
    /// Execute a state graph with a task using a real LLM provider
    Run {
        /// State graph name
        name: String,
        /// The task to execute
        task: String,
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
        /// Model to use
        #[arg(long)]
        model: Option<String>,
        /// User id
        #[arg(long)]
        user: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    let config = config::OneaiConfig::load_or_default();

    match cli.command {
        None => {
            // Default: launch TUI (same as "oneai chat" with no options)
            cmd_chat::cmd_chat(&config, None, None, None);
        }
        Some(Commands::Chat { domain, model, user }) => {
            cmd_chat::cmd_chat(&config, domain.as_deref(), model.as_deref(), user.as_deref());
        }
        Some(Commands::Run { prompt, domain, model, user }) => {
            cmd_run::cmd_run(&prompt, &config, domain.as_deref(), model.as_deref(), user.as_deref());
        }
        Some(Commands::Studio { port, domain, model, user }) => {
            cmd_studio::cmd_studio(&config, port, domain.as_deref(), model.as_deref(), user.as_deref());
        }
        Some(Commands::Pack { action }) => {
            match action {
                PackAction::List => cmd_pack::cmd_pack_list(),
                PackAction::Show { name } => cmd_pack::cmd_pack_show(&name),
                PackAction::Install { source } => cmd_pack::cmd_pack_install(&source),
                PackAction::Validate { path } => cmd_pack::cmd_pack_validate(&path),
                PackAction::Spec => cmd_pack::cmd_pack_spec(),
                PackAction::Check { name } => cmd_pack::cmd_pack_check(&name),
            }
        }
        Some(Commands::Skill { action }) => {
            match action {
                SkillAction::List => cmd_skill::cmd_skill_list(),
                SkillAction::Show { name } => cmd_skill::cmd_skill_show(&name),
            }
        }
        Some(Commands::Eval { action }) => {
            match action {
                EvalAction::List => cmd_eval::cmd_eval_list(),
                EvalAction::Run { name, format, profile, record } => {
                    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                    rt.block_on(cmd_eval::cmd_eval_run(&name, &format, profile, record));
                }
                EvalAction::Score { name } => {
                    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                    rt.block_on(cmd_eval::cmd_eval_score(&name));
                }
                EvalAction::Replay { path } => {
                    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                    rt.block_on(cmd_eval::cmd_eval_replay(&path));
                }
                EvalAction::Swebench {
                    dataset,
                    instances,
                    workspace,
                    python,
                    modal,
                    dataset_name,
                    limit,
                    format,
                    run_id,
                } => {
                    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                    rt.block_on(cmd_eval::cmd_eval_swebench(
                        &dataset,
                        instances.as_deref(),
                        &workspace,
                        python.as_deref(),
                        modal,
                        &dataset_name,
                        limit,
                        &format,
                        &run_id,
                    ));
                }
                EvalAction::Memory {
                    suite,
                    data,
                    metrics,
                    no_embedding,
                    k,
                    format,
                } => {
                    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                    rt.block_on(cmd_eval::cmd_eval_memory(
                        &suite, data.as_deref(), &metrics, no_embedding, k, &format,
                    ));
                }
            }
        }
        Some(Commands::Config { action }) => {
            match action {
                ConfigAction::Show => cmd_config::cmd_config_show(),
                ConfigAction::Init => cmd_config::cmd_config_init(),
            }
        }
        Some(Commands::Mcp { action }) => {
            match action {
                McpAction::Serve { domain } => {
                    cmd_mcp::cmd_mcp_serve(domain.as_deref());
                }
                McpAction::List => cmd_mcp::cmd_mcp_list(),
                McpAction::Add { name, transport, command, url, args, enabled } => {
                    cmd_mcp::cmd_mcp_add(&name, &transport, command.as_deref(), url.as_deref(), args.as_deref(), enabled);
                }
                McpAction::Remove { name } => {
                    cmd_mcp::cmd_mcp_remove(&name);
                }
                McpAction::Connect { name } => {
                    cmd_mcp::cmd_mcp_connect(&name);
                }
            }
        }
        Some(Commands::A2a { action }) => {
            match action {
                A2aAction::Serve { domain } => {
                    cmd_a2a::cmd_a2a_serve(domain.as_deref());
                }
                A2aAction::Discover { url } => {
                    cmd_a2a::cmd_a2a_discover(&url);
                }
                A2aAction::List => {
                    cmd_a2a::cmd_a2a_list();
                }
                A2aAction::Send { url, message } => {
                    cmd_a2a::cmd_a2a_send(&url, &message);
                }
            }
        }
        Some(Commands::Wasm { action }) => {
            match action {
                WasmAction::List => cmd_wasm::cmd_wasm_list(),
                WasmAction::Load { name, file } => cmd_wasm::cmd_wasm_load(&name, &file),
                WasmAction::Run { name, input, input_file } => {
                    cmd_wasm::cmd_wasm_run(&name, input.as_deref(), input_file.as_deref());
                }
                WasmAction::Health { name } => {
                    cmd_wasm::cmd_wasm_health(name.as_deref());
                }
                WasmAction::Unload { name } => cmd_wasm::cmd_wasm_unload(&name),
                WasmAction::Stats => cmd_wasm::cmd_wasm_stats(),
            }
        }
        Some(Commands::Session { action }) => {
            match action {
                SessionAction::List => cmd_session::cmd_session_list(),
                SessionAction::Resume { id } => cmd_session::cmd_session_resume(&id),
                SessionAction::Delete { id } => cmd_session::cmd_session_delete(&id),
                SessionAction::Info { id } => cmd_session::cmd_session_info(&id),
            }
        }
        Some(Commands::Tasks { action }) => match action {
            TasksAction::List { user, root } => {
                cmd_tasks::cmd_tasks_list(user.as_deref(), root.as_deref())
            }
            TasksAction::Show { id, root } => cmd_tasks::cmd_tasks_show(&id, root.as_deref()),
            TasksAction::Continue { id, domain, model, user, root } => cmd_tasks::cmd_tasks_continue(
                &id, &config, domain.as_deref(), model.as_deref(), user.as_deref(), root.as_deref(),
            ),
            TasksAction::Archive { id, root } => cmd_tasks::cmd_tasks_archive(&id, root.as_deref()),
        },
        Some(Commands::Memory { action }) => {
            match action {
                MemoryAction::Search { query, user, top_k } => {
                    cmd_memory::cmd_memory_search(&query, &user, top_k);
                }
                MemoryAction::List { user, session } => {
                    cmd_memory::cmd_memory_list(&user, session.as_deref());
                }
            }
        }
        Some(Commands::Embed { action }) => {
            match action {
                EmbedAction::Generate { text, model, service, api_key } => {
                    cmd_embed::cmd_embed_generate(&text, model.as_deref(), service.as_deref(), api_key.as_deref());
                }
                EmbedAction::Batch { texts, model, service, api_key } => {
                    cmd_embed::cmd_embed_batch(&texts, model.as_deref(), service.as_deref(), api_key.as_deref());
                }
                EmbedAction::List => cmd_embed::cmd_embed_list(),
                EmbedAction::Health { model, service, api_key } => {
                    cmd_embed::cmd_embed_health(model.as_deref(), service.as_deref(), api_key.as_deref());
                }
                EmbedAction::Dimension { model, service, api_key } => {
                    cmd_embed::cmd_embed_dimension(model.as_deref(), service.as_deref(), api_key.as_deref());
                }
            }
        }
        Some(Commands::Usage { action }) => {
            match action {
                UsageAction::Report => cmd_usage::cmd_usage_report(),
                UsageAction::Session { id } => cmd_usage::cmd_usage_session(&id),
                UsageAction::Export { format } => cmd_usage::cmd_usage_export(&format),
            }
        }
        Some(Commands::Provider { action }) => {
            match action {
                ProviderAction::Status => {
                    cmd_provider::run_provider_status();
                }
                ProviderAction::FallbackLog { limit } => {
                    cmd_provider::run_fallback_log_with_limit(&limit);
                }
                ProviderAction::Test => {
                    cmd_provider::run_provider_test();
                }
                ProviderAction::Route { task, strategy } => {
                    cmd_provider::run_route_dry_run(&task, &strategy);
                }
                ProviderAction::RouteLog { limit } => {
                    cmd_provider::run_route_log(&limit);
                }
                ProviderAction::RouteConfig => {
                    cmd_provider::run_route_config();
                }
            }
        }
        Some(Commands::Token { action }) => {
            match action {
                TokenAction::Count { text, model } => {
                    cmd_token::run_token_count(&text, model.as_deref());
                }
                TokenAction::Estimate { model } => {
                    cmd_token::run_token_estimate(model.as_deref());
                }
                TokenAction::Context { model } => {
                    cmd_token::run_token_context(&model);
                }
                TokenAction::Models => {
                    cmd_token::run_token_models();
                }
                TokenAction::Fits { text, model } => {
                    cmd_token::run_token_fits(&text, &model);
                }
                TokenAction::Probe { model } => {
                    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                    rt.block_on(cmd_token::run_token_probe(model.as_deref(), &config));
                }
            }
        }
        Some(Commands::Team { action }) => {
            match action {
                TeamAction::Strategies => cmd_team::cmd_team_strategies(),
                TeamAction::Presets => cmd_team::cmd_team_presets(),
                TeamAction::Info { id } => cmd_team::cmd_team_info(&id),
                TeamAction::Run { task, strategy, preset, budget } => {
                    cmd_team::cmd_team_run(&task, &strategy, preset.as_deref(), budget.as_deref());
                }
            }
        }
        Some(Commands::Handoff { action }) => {
            match action {
                HandoffAction::List => cmd_handoff::cmd_handoff_list(),
                HandoffAction::Targets { preset } => cmd_handoff::cmd_handoff_targets(&preset),
                HandoffAction::Config { preset } => cmd_handoff::cmd_handoff_config(preset.as_deref()),
                HandoffAction::Run { target, reason, preset } => {
                    cmd_handoff::cmd_handoff_run(&target, &reason, preset.as_deref());
                }
            }
        }
        Some(Commands::Swarm { action }) => {
            match action {
                SwarmAction::List => cmd_swarm::cmd_swarm_list(),
                SwarmAction::Routing => cmd_swarm::cmd_swarm_routing(),
                SwarmAction::Config { preset } => cmd_swarm::cmd_swarm_config(&preset),
                SwarmAction::Agents { preset } => cmd_swarm::cmd_swarm_agents(&preset),
                SwarmAction::Run { task, routing, preset, budget } => {
                    cmd_swarm::cmd_swarm_run(&task, &routing, preset.as_deref(), budget.as_deref());
                }
            }
        }
        Some(Commands::Workflow { action }) => {
            match action {
                WorkflowAction::List { domain } => {
                    cmd_workflow::cmd_workflow_list(&config, domain.as_deref());
                }
                WorkflowAction::Show { name, domain } => {
                    cmd_workflow::cmd_workflow_show(&name, &config, domain.as_deref());
                }
                WorkflowAction::Run { name, task, domain, model, user } => {
                    cmd_workflow::cmd_workflow_run(
                        &name,
                        task.as_deref(),
                        &config,
                        domain.as_deref(),
                        model.as_deref(),
                        user.as_deref(),
                    );
                }
            }
        }
        Some(Commands::Graph { action }) => {
            match action {
                GraphAction::List { domain } => {
                    cmd_workflow::cmd_graph_list(&config, domain.as_deref());
                }
                GraphAction::Show { name, domain } => {
                    cmd_workflow::cmd_graph_show(&name, &config, domain.as_deref());
                }
                GraphAction::Run { name, task, domain, model, user } => {
                    cmd_workflow::cmd_graph_run(
                        &name,
                        &task,
                        &config,
                        domain.as_deref(),
                        model.as_deref(),
                        user.as_deref(),
                    );
                }
            }
        }
        Some(Commands::Version) => {
            cmd_version::cmd_version();
        }
        Some(Commands::Init { format, path, force, no_llm }) => {
            cmd_init::cmd_init(&config, Some(&format), path.as_deref(), force, no_llm);
        }
    }
}
