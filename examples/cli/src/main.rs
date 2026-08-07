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
//!   oneai gateway serve   — Start the message-platform webhook server (Feishu/WeChat/loopback)
//!   oneai gateway channels — List bound channels
//!   oneai cron add/list/rm/fire/serve — Durable NL/cron scheduling + external one-shot triggers (Phase 3.2)
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

mod cmd_a2a;
mod cmd_chat;
mod cmd_config;
mod cmd_cron;
mod cmd_curator;
mod cmd_embed;
mod cmd_eval;
mod cmd_evolve;
mod cmd_gateway;
mod cmd_init;
mod cmd_mcp;
mod cmd_memory;
mod cmd_pack;
mod cmd_provider;
mod cmd_reload;
mod cmd_run;
mod cmd_session;
mod cmd_skill;
mod cmd_studio;
mod cmd_supervisor;
mod cmd_tasks;
mod cmd_terminal;
mod cmd_token;
mod cmd_usage;
mod cmd_version;
mod cmd_wasm;
mod cmd_workflow;
mod config;
mod tui;

use clap::{ArgAction, Parser, Subcommand};

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
    /// Reload the agent's runtime data layer (skills / MCP tools) without
    /// restarting (Phase 3.4). Re-reads convention-dir skill markdown and
    /// re-registers MCP tools; the next `run`/`chat` sees them.
    Reload {
        /// Domain pack to use (selects builtin skills to register)
        #[arg(long)]
        domain: Option<String>,
        /// Model name override (overrides ONEAI_MODEL / config)
        #[arg(long)]
        model: Option<String>,
        /// User identity for memory namespacing
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
    /// Message gateway — expose OneAI as a Feishu / WeChat / loopback bot over
    /// webhooks (Phase 3.1)
    Gateway {
        #[command(subcommand)]
        action: GatewayAction,
    },
    /// Supervise long-lived AgentLoop instances over IPC (headless daemon)
    Supervisor {
        #[command(subcommand)]
        action: SupervisorAction,
    },
    /// Cron — durable NL/cron/ISO scheduling + external one-shot triggers
    /// (Phase 3.2). Deliver fired jobs into the gateway's bound channel
    /// sessions (`deliver=origin`).
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },
    /// Terminal — `TerminalBackend` management (Phase 3.3). Run commands
    /// through a local / docker / serverless backend, and exercise the
    /// snapshot / restore / cleanup(hibernate) lifecycle.
    Terminal {
        #[command(subcommand)]
        action: TerminalAction,
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
    /// Run the skill lifecycle curator — status / run / pin / archive /
    /// restore / backup / rollback (Phase 2.1 Stage B). The closed-loop
    /// steward that retires stale skills (never deletes — only archives,
    /// restorable via backups).
    Curator {
        #[command(subcommand)]
        action: CuratorAction,
        /// Domain pack (drives the `skill_lifecycle` policy; default: coding)
        #[arg(long)]
        domain: Option<String>,
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
    /// Self-evolution loop — trajectory collection + EDD scoring (E1: run only;
    /// diagnosis/variation/Pareto land in E2–E3).
    Evolve {
        #[command(subcommand)]
        action: EvolveAction,
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
    /// Build a ContainerizedCodingPack — VM-backed shell + file tools (Gondolin
    /// tool-override, evolution-plan §4.2). Prints the tool wiring for a
    /// chosen backend so the same shell/read_file/edit_file/write_file/list_directory
    /// route their side-effects through a Docker/Modal/Daytona terminal.
    Containerized {
        /// Terminal backend name (local / docker / modal / daytona)
        #[arg(long, default_value = "docker")]
        backend: String,
        /// Project directory (CodingPack context sources are rooted here)
        #[arg(long, default_value = ".")]
        project_dir: String,
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
enum CuratorAction {
    /// Show every skill's lifecycle state / use_count / pinned / author
    Status,
    /// Apply automatic Active→Stale→Archived transitions (writes a backup
    /// before any retirement), then print a report.
    Run,
    /// Pin a skill (exempt from automatic retirement)
    Pin {
        /// Skill name
        name: String,
    },
    /// Unpin a skill
    Unpin {
        /// Skill name
        name: String,
    },
    /// Manually archive a skill (reversible — a backup is written first)
    Archive {
        /// Skill name
        name: String,
    },
    /// Restore an archived skill to Active
    Restore {
        /// Skill name
        name: String,
    },
    /// Write a restorable snapshot of every skill the agent sees
    Backup,
    /// List available backup snapshot ids (unix timestamps, newest first)
    Backups,
    /// Restore a backup snapshot (skills + metadata) by id
    Rollback {
        /// Backup snapshot id (unix timestamp — see `backups`)
        id: u64,
    },
    /// LLM consolidation pass (default-off, opt-in) — merge narrow skills into
    /// class-level umbrella skills. Needs a configured LLM provider (ONEAI_API_KEY
    /// or ~/.oneai/config.toml), unlike the other curator actions. Each merge is
    /// reversible via `oneai curator rollback <id>` (printed in the report).
    Consolidate,
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
enum EvolveAction {
    /// Run generation 0 (E1): hot-load the seed pack, score against a builtin
    /// suite, persist a report + per-case trajectories. No optimization.
    Run {
        /// Path to the seed DomainPack config (YAML/TOML).
        #[arg(long)]
        seed: String,
        /// Builtin suite name (coding_basics, tool_use, general, efficiency).
        /// Required unless `--suite-file` is given.
        #[arg(long)]
        suite: Option<String>,
        /// Path to a GSM8K-format JSONL suite file (`{"question","answer"}`).
        /// Takes priority over `--suite`. Pair with `--sample N` to subset.
        #[arg(long)]
        suite_file: Option<String>,
        /// When loading `--suite-file`, take a deterministic random sample of
        /// the first N cases (full file by default). Useful for GSM8K's 8.5k.
        #[arg(long)]
        sample: Option<usize>,
        /// Skip variation (E1/E2 degenerate path; default true). Pass
        /// `--no-optimize false` to run the GEPA variation + Pareto loop.
        #[arg(long, default_value = "true", action = ArgAction::Set)]
        no_optimize: bool,
        /// E4: generation cap. 1 = single-gen (E3); >1 = multi-gen loop to
        /// convergence.
        #[arg(long, default_value = "1")]
        max_generations: usize,
        /// E4: convergence target pass rate (0.0–1.0). Loop stops once the
        /// frontier-best reaches it.
        #[arg(long, default_value = "0.85")]
        target: f64,
        /// E4: early-stop patience — consecutive generations with no frontier
        /// improvement before stopping.
        #[arg(long, default_value = "2")]
        patience: usize,
        /// E4: cumulative token cap across all generations (budget hard-stop).
        #[arg(long)]
        max_tokens: Option<u64>,
        /// Output root (default ~/.oneai/evolve).
        #[arg(long)]
        root: Option<String>,
        /// E5: dedicated variation/judge model name (design §6.3 separation).
        /// Absent → the candidate provider doubles as the variation provider
        /// (single-model smoke harness).
        #[arg(long)]
        judge_model: Option<String>,
        /// Output format (markdown, json).
        #[arg(long, default_value = "markdown")]
        format: String,
    },
    /// E5: resume an existing run-dir for one more generation. Reads
    /// `report.json` for the gen index + no_optimize flag, loads the latest
    /// frontier config (or seed.json) as the new base, appends a lesson row.
    Step {
        /// Run directory (the `run-<ts>` under `~/.oneai/evolve`).
        run_dir: String,
        /// Builtin suite name (must match the original run).
        #[arg(long)]
        suite: String,
        /// E5: dedicated variation/judge model (only used if the original run
        /// was optimized).
        #[arg(long)]
        judge_model: Option<String>,
        /// Output format (markdown, json).
        #[arg(long, default_value = "markdown")]
        format: String,
    },
    /// E5: pretty-print a persisted run's `report.json`.
    Report {
        /// Run directory.
        run_dir: String,
        /// Output format (markdown, json).
        #[arg(long, default_value = "markdown")]
        format: String,
    },
    /// E5: structured config diff between the seed and a generation's frontier
    /// (`frontier-gen<N>.json` vs `seed.json`).
    Diff {
        /// Run directory.
        run_dir: String,
        /// Frontier generation index (default: the latest persisted).
        #[arg(long)]
        gen: Option<usize>,
        /// Override the seed config file (default: `<run-dir>/seed.json`).
        #[arg(long)]
        seed: Option<String>,
        /// Output format (markdown, json).
        #[arg(long, default_value = "markdown")]
        format: String,
    },
    /// E5: print the cross-generation `lessons.jsonl`.
    Lesson {
        /// Run directory.
        run_dir: String,
        /// Output format (markdown, json).
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
        /// Port to bind (default: 8080)
        #[arg(long, default_value_t = 8080)]
        port: u16,
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
enum GatewayAction {
    /// Start the gateway webhook server (Feishu / WeChat / loopback)
    Serve {
        /// Address to bind (default: 0.0.0.0:9090)
        #[arg(long, default_value = "0.0.0.0:9090")]
        bind: String,
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
    /// List bound channels (platform → session id) from the directory
    Channels,
    /// Install / manage the macOS LaunchAgent that auto-starts the supervisor
    /// (with inlined gateway) at login — §3.1 Part F.
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
}

#[derive(Subcommand)]
enum AutostartAction {
    /// Write the LaunchAgent plist + `launchctl load` (runs `supervisor serve
    /// --with-gateway` on login, kept alive across crashes).
    Install,
    /// `launchctl unload` + remove the plist.
    Uninstall,
    /// Show whether the LaunchAgent is loaded.
    Status,
}

#[derive(Subcommand)]
enum CronAction {
    /// Add a scheduled cron job.
    Add {
        /// Job name.
        #[arg(long)]
        name: String,
        /// Schedule: `"30m"` / `"every 2h"` / ISO `"2026-08-01T09:00:00Z"` /
        /// 5-field cron `"0 9 * * *"`.
        #[arg(long)]
        schedule: String,
        /// The task / prompt to deliver into the agent turn.
        #[arg(long)]
        task: String,
        /// Originating platform name (for `deliver=origin`).
        #[arg(long, default_value = "loopback")]
        platform: String,
        /// Originating channel (raw) to deliver the reply to.
        #[arg(long)]
        channel: Option<String>,
        /// Session id to deliver into (defaults to a fresh id bound to the
        /// channel on first fire).
        #[arg(long)]
        session: Option<String>,
        /// Bound DomainPack (carried via SESSION_SOURCE for the lazily-built
        /// App factory). Default: coding.
        #[arg(long)]
        pack: Option<String>,
        /// Delivery mode: `origin` (relay reply to the channel) or `silent`
        /// (run the turn, log only). Default: origin.
        #[arg(long, default_value = "origin")]
        deliver: String,
    },
    /// List scheduled cron jobs.
    List,
    /// Remove a cron job by id.
    Rm {
        /// Job id.
        id: String,
    },
    /// Manually fire a cron job now (force — ignores the due window; routes
    /// through the same CAS path so it can't double-fire the current window).
    Fire {
        /// Job id.
        id: String,
    },
    /// Start the cron orchestrator ticker + the external `/cron/fire`
    /// one-shot receiver. Reuses the gateway for delivery (`deliver=origin`)
    /// — builds a real `App` + the gateway, wires a `CronRunner` that calls
    /// `gateway.deliver_scheduled(...)`, and starts the scheduler + HTTP
    /// receiver.
    Serve {
        /// Address for the `/cron/fire` receiver (default: 0.0.0.0:9091).
        #[arg(long, default_value = "0.0.0.0:9091")]
        cron_bind: String,
        /// Gateway webhook bind (the gateway runs in-process for delivery).
        #[arg(long, default_value = "0.0.0.0:9090")]
        gateway_bind: String,
        /// Domain pack.
        #[arg(long)]
        domain: Option<String>,
        /// Model override.
        #[arg(long)]
        model: Option<String>,
        /// User identity for memory namespacing.
        #[arg(long)]
        user: Option<String>,
    },
}

#[derive(Subcommand)]
enum TerminalAction {
    /// List available terminal backends (local / docker / modal / daytona).
    List,
    /// Execute a one-off command through a named backend.
    Exec {
        /// Backend name: `local` / `docker` / `modal` / `daytona`.
        #[arg(long)]
        backend: String,
        /// The shell command to execute.
        command: String,
        /// Timeout in seconds (default 120).
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// Max output size in bytes (default 100_000).
        #[arg(long, default_value_t = 100_000)]
        max_output: usize,
    },
    /// Snapshot the backend's session state (returns a restorable id).
    Snapshot {
        #[arg(long)]
        backend: String,
    },
    /// Restore the backend from a snapshot id.
    Restore {
        #[arg(long)]
        backend: String,
        /// Snapshot id returned by `snapshot`.
        id: String,
    },
    /// Tear down the backend. `--hibernate` stops+keeps (restorable); without
    /// it the state is destroyed.
    Cleanup {
        #[arg(long)]
        backend: String,
        #[arg(long)]
        hibernate: bool,
    },
}

#[derive(Subcommand)]
enum SupervisorAction {
    /// Start the headless supervisor daemon (serves the IPC socket)
    Serve {
        /// IPC socket path (default: ~/.oneai/server.sock)
        #[arg(long)]
        socket: Option<String>,
        /// Default domain pack for spawned instances
        #[arg(long)]
        domain: Option<String>,
        /// Default model override
        #[arg(long)]
        model: Option<String>,
        /// Default user identity for memory namespacing
        #[arg(long)]
        user: Option<String>,
        /// Also bring up the message gateway (Feishu/WeChat webhook + Feishu
        /// long-connection) in this process — the auto-start path: a LaunchAgent
        /// runs `supervisor serve --with-gateway` so the gateway comes up on
        /// boot without a separate command (§3.1 Part E/F).
        #[arg(long, default_value_t = false)]
        with_gateway: bool,
        /// Bind address for the inlined gateway webhook (default: 0.0.0.0:9090)
        #[arg(long)]
        gateway_bind: Option<String>,
    },
    /// List supervised instances on a running daemon
    List {
        #[arg(long)]
        socket: Option<String>,
    },
    /// Spawn a new supervised instance on a running daemon
    Spawn {
        /// Instance id
        id: String,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Stop an instance on a running daemon
    Stop {
        /// Instance id
        id: String,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Query an instance's status on a running daemon
    Status {
        /// Instance id
        id: String,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Run one agent turn on an instance (request-response)
    Rpc {
        /// Instance id
        id: String,
        /// Task message
        message: String,
        #[arg(long)]
        socket: Option<String>,
    },
    /// Run one agent turn on an instance, streaming live events
    RpcStream {
        /// Instance id
        id: String,
        /// Task message
        message: String,
        #[arg(long)]
        socket: Option<String>,
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
    /// Phase 2.4 memory decay pass (gap P2 #16) — page low-salience core
    /// facts to the archive + soft-invalidate stale low-salience archival
    /// facts ("forgotten but auditable"). Needs SQLite persistence + a
    /// domain whose `MemoryProfile.decay.enabled` is true (research/assistant).
    Decay {
        /// User id whose durable fact base to sweep (default: "default")
        #[arg(long)]
        user: Option<String>,
        /// Domain pack (drives the decay policy; default: coding)
        #[arg(long)]
        domain: Option<String>,
    },
    /// Export a saved session to HuggingFace-dataset JSONL (Phase 3.6).
    /// Stitches live + archived transcript, redacts secrets, optionally
    /// attaches a task's working-state event log.
    ExportHf {
        /// Session ID to export
        id: String,
        /// Output .jsonl path
        #[arg(short = 'o', long)]
        out: String,
        /// Attach a task's working-state event log (task id)
        #[arg(long)]
        task: Option<String>,
        /// Working-state root (default: .oneai)
        #[arg(long, default_value = ".oneai")]
        ws_root: String,
        /// Also redact private/loopback IPv4 addresses
        #[arg(long)]
        redact_ips: bool,
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
    /// List the generated model catalog with capability flags (L3 authority)
    Catalog,
    /// Detect & show the Compat profile for a base_url (provider dispatch)
    Compat {
        /// Base URL to detect the compatibility family for
        base_url: String,
    },
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
        Some(Commands::Chat {
            domain,
            model,
            user,
        }) => {
            cmd_chat::cmd_chat(
                &config,
                domain.as_deref(),
                model.as_deref(),
                user.as_deref(),
            );
        }
        Some(Commands::Run {
            prompt,
            domain,
            model,
            user,
        }) => {
            cmd_run::cmd_run(
                &prompt,
                &config,
                domain.as_deref(),
                model.as_deref(),
                user.as_deref(),
            );
        }
        Some(Commands::Reload {
            domain,
            model,
            user,
        }) => {
            cmd_reload::cmd_reload(
                &config,
                domain.as_deref(),
                model.as_deref(),
                user.as_deref(),
            );
        }
        Some(Commands::Studio {
            port,
            domain,
            model,
            user,
        }) => {
            cmd_studio::cmd_studio(
                &config,
                port,
                domain.as_deref(),
                model.as_deref(),
                user.as_deref(),
            );
        }
        Some(Commands::Gateway { action }) => match action {
            GatewayAction::Serve {
                bind,
                domain,
                model,
                user,
            } => {
                cmd_gateway::cmd_gateway_serve(
                    &config,
                    &bind,
                    domain.as_deref(),
                    model.as_deref(),
                    user.as_deref(),
                );
            }
            GatewayAction::Channels => {
                cmd_gateway::cmd_gateway_channels();
            }
            GatewayAction::Autostart { action } => match action {
                AutostartAction::Install => cmd_gateway::cmd_gateway_autostart_install(),
                AutostartAction::Uninstall => cmd_gateway::cmd_gateway_autostart_uninstall(),
                AutostartAction::Status => cmd_gateway::cmd_gateway_autostart_status(),
            },
        },
        Some(Commands::Cron { action }) => match action {
            CronAction::Add {
                name,
                schedule,
                task,
                platform,
                channel,
                session,
                pack,
                deliver,
            } => cmd_cron::cmd_cron_add(
                &name,
                &schedule,
                &task,
                &platform,
                channel.as_deref(),
                session.as_deref(),
                pack.as_deref(),
                &deliver,
            ),
            CronAction::List => cmd_cron::cmd_cron_list(),
            CronAction::Rm { id } => cmd_cron::cmd_cron_rm(&id),
            CronAction::Fire { id } => cmd_cron::cmd_cron_fire(&id),
            CronAction::Serve {
                cron_bind,
                gateway_bind,
                domain,
                model,
                user,
            } => cmd_cron::cmd_cron_serve(
                &config,
                &cron_bind,
                &gateway_bind,
                domain.as_deref(),
                model.as_deref(),
                user.as_deref(),
            ),
        },
        Some(Commands::Terminal { action }) => match action {
            TerminalAction::List => cmd_terminal::cmd_terminal_list(),
            TerminalAction::Exec {
                backend,
                command,
                timeout,
                max_output,
            } => cmd_terminal::cmd_terminal_exec(&backend, &command, timeout, max_output),
            TerminalAction::Snapshot { backend } => cmd_terminal::cmd_terminal_snapshot(&backend),
            TerminalAction::Restore { backend, id } => {
                cmd_terminal::cmd_terminal_restore(&backend, &id)
            }
            TerminalAction::Cleanup { backend, hibernate } => {
                cmd_terminal::cmd_terminal_cleanup(&backend, hibernate)
            }
        },
        Some(Commands::Supervisor { action }) => match action {
            SupervisorAction::Serve {
                socket,
                domain,
                model,
                user,
                with_gateway,
                gateway_bind,
            } => cmd_supervisor::cmd_supervisor_serve(
                &config,
                socket.as_deref(),
                domain.as_deref(),
                model.as_deref(),
                user.as_deref(),
                with_gateway,
                gateway_bind.as_deref(),
            ),
            SupervisorAction::List { socket } => {
                cmd_supervisor::cmd_supervisor_list(socket.as_deref())
            }
            SupervisorAction::Spawn {
                id,
                domain,
                model,
                user,
                socket,
            } => cmd_supervisor::cmd_supervisor_spawn(
                socket.as_deref(),
                &id,
                domain.as_deref(),
                model.as_deref(),
                user.as_deref(),
            ),
            SupervisorAction::Stop { id, socket } => {
                cmd_supervisor::cmd_supervisor_stop(socket.as_deref(), &id)
            }
            SupervisorAction::Status { id, socket } => {
                cmd_supervisor::cmd_supervisor_status(socket.as_deref(), &id)
            }
            SupervisorAction::Rpc {
                id,
                message,
                socket,
            } => cmd_supervisor::cmd_supervisor_rpc(socket.as_deref(), &id, &message),
            SupervisorAction::RpcStream {
                id,
                message,
                socket,
            } => cmd_supervisor::cmd_supervisor_rpc_stream(socket.as_deref(), &id, &message),
        },
        Some(Commands::Pack { action }) => match action {
            PackAction::List => cmd_pack::cmd_pack_list(),
            PackAction::Show { name } => cmd_pack::cmd_pack_show(&name),
            PackAction::Install { source } => cmd_pack::cmd_pack_install(&source),
            PackAction::Validate { path } => cmd_pack::cmd_pack_validate(&path),
            PackAction::Spec => cmd_pack::cmd_pack_spec(),
            PackAction::Check { name } => cmd_pack::cmd_pack_check(&name),
            PackAction::Containerized {
                backend,
                project_dir,
            } => cmd_pack::cmd_pack_containerized(&backend, &project_dir),
        },
        Some(Commands::Skill { action }) => match action {
            SkillAction::List => cmd_skill::cmd_skill_list(),
            SkillAction::Show { name } => cmd_skill::cmd_skill_show(&name),
        },
        Some(Commands::Curator { action, domain }) => {
            let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
            rt.block_on(async move {
                match action {
                    CuratorAction::Status => {
                        cmd_curator::cmd_curator_status(&config, domain.as_deref()).await
                    }
                    CuratorAction::Run => {
                        cmd_curator::cmd_curator_run(&config, domain.as_deref()).await
                    }
                    CuratorAction::Pin { name } => {
                        cmd_curator::cmd_curator_pin(&config, domain.as_deref(), &name, true).await
                    }
                    CuratorAction::Unpin { name } => {
                        cmd_curator::cmd_curator_pin(&config, domain.as_deref(), &name, false).await
                    }
                    CuratorAction::Archive { name } => {
                        cmd_curator::cmd_curator_archive(&config, domain.as_deref(), &name).await
                    }
                    CuratorAction::Restore { name } => {
                        cmd_curator::cmd_curator_restore(&config, domain.as_deref(), &name).await
                    }
                    CuratorAction::Backup => {
                        cmd_curator::cmd_curator_backup(&config, domain.as_deref()).await
                    }
                    CuratorAction::Backups => {
                        cmd_curator::cmd_curator_backups(&config, domain.as_deref()).await
                    }
                    CuratorAction::Rollback { id } => {
                        cmd_curator::cmd_curator_rollback(&config, domain.as_deref(), id).await
                    }
                    CuratorAction::Consolidate => {
                        cmd_curator::cmd_curator_consolidate(&config, domain.as_deref()).await
                    }
                }
            });
        }
        Some(Commands::Eval { action }) => match action {
            EvalAction::List => cmd_eval::cmd_eval_list(),
            EvalAction::Run {
                name,
                format,
                profile,
                record,
            } => {
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
                    &suite,
                    data.as_deref(),
                    &metrics,
                    no_embedding,
                    k,
                    &format,
                ));
            }
        },
        Some(Commands::Evolve { action }) => match action {
            EvolveAction::Run {
                seed,
                suite,
                suite_file,
                sample,
                no_optimize,
                max_generations,
                target,
                patience,
                max_tokens,
                root,
                judge_model,
                format,
            } => {
                let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                rt.block_on(cmd_evolve::cmd_evolve_run(
                    &seed,
                    suite.as_deref(),
                    suite_file.as_deref(),
                    sample,
                    no_optimize,
                    max_generations,
                    target,
                    patience,
                    max_tokens,
                    root.as_deref(),
                    judge_model.as_deref(),
                    &format,
                ));
            }
            EvolveAction::Step {
                run_dir,
                suite,
                judge_model,
                format,
            } => {
                let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                rt.block_on(cmd_evolve::cmd_evolve_step(
                    &run_dir,
                    &suite,
                    judge_model.as_deref(),
                    &format,
                ));
            }
            EvolveAction::Report { run_dir, format } => {
                cmd_evolve::cmd_evolve_report(&run_dir, &format);
            }
            EvolveAction::Diff {
                run_dir,
                gen,
                seed,
                format,
            } => {
                cmd_evolve::cmd_evolve_diff(&run_dir, gen, seed.as_deref(), &format);
            }
            EvolveAction::Lesson { run_dir, format } => {
                cmd_evolve::cmd_evolve_lesson(&run_dir, &format);
            }
        },
        Some(Commands::Config { action }) => match action {
            ConfigAction::Show => cmd_config::cmd_config_show(),
            ConfigAction::Init => cmd_config::cmd_config_init(),
        },
        Some(Commands::Mcp { action }) => match action {
            McpAction::Serve { domain } => {
                cmd_mcp::cmd_mcp_serve(domain.as_deref());
            }
            McpAction::List => cmd_mcp::cmd_mcp_list(),
            McpAction::Add {
                name,
                transport,
                command,
                url,
                args,
                enabled,
            } => {
                cmd_mcp::cmd_mcp_add(
                    &name,
                    &transport,
                    command.as_deref(),
                    url.as_deref(),
                    args.as_deref(),
                    enabled,
                );
            }
            McpAction::Remove { name } => {
                cmd_mcp::cmd_mcp_remove(&name);
            }
            McpAction::Connect { name } => {
                cmd_mcp::cmd_mcp_connect(&name);
            }
        },
        Some(Commands::A2a { action }) => match action {
            A2aAction::Serve { domain, port } => {
                cmd_a2a::cmd_a2a_serve(domain.as_deref(), port);
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
        },
        Some(Commands::Wasm { action }) => match action {
            WasmAction::List => cmd_wasm::cmd_wasm_list(),
            WasmAction::Load { name, file } => cmd_wasm::cmd_wasm_load(&name, &file),
            WasmAction::Run {
                name,
                input,
                input_file,
            } => {
                cmd_wasm::cmd_wasm_run(&name, input.as_deref(), input_file.as_deref());
            }
            WasmAction::Health { name } => {
                cmd_wasm::cmd_wasm_health(name.as_deref());
            }
            WasmAction::Unload { name } => cmd_wasm::cmd_wasm_unload(&name),
            WasmAction::Stats => cmd_wasm::cmd_wasm_stats(),
        },
        Some(Commands::Session { action }) => match action {
            SessionAction::List => cmd_session::cmd_session_list(),
            SessionAction::Resume { id } => cmd_session::cmd_session_resume(&id),
            SessionAction::Delete { id } => cmd_session::cmd_session_delete(&id),
            SessionAction::Info { id } => cmd_session::cmd_session_info(&id),
            SessionAction::Decay { user, domain } => {
                let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                rt.block_on(cmd_session::cmd_session_decay(
                    &config,
                    user.as_deref(),
                    domain.as_deref(),
                ));
            }
            SessionAction::ExportHf {
                id,
                out,
                task,
                ws_root,
                redact_ips,
            } => cmd_session::cmd_session_export_hf(
                &id,
                std::path::Path::new(&out),
                task.as_deref(),
                std::path::Path::new(&ws_root),
                redact_ips,
            ),
        },
        Some(Commands::Tasks { action }) => match action {
            TasksAction::List { user, root } => {
                cmd_tasks::cmd_tasks_list(user.as_deref(), root.as_deref())
            }
            TasksAction::Show { id, root } => cmd_tasks::cmd_tasks_show(&id, root.as_deref()),
            TasksAction::Continue {
                id,
                domain,
                model,
                user,
                root,
            } => cmd_tasks::cmd_tasks_continue(
                &id,
                &config,
                domain.as_deref(),
                model.as_deref(),
                user.as_deref(),
                root.as_deref(),
            ),
            TasksAction::Archive { id, root } => cmd_tasks::cmd_tasks_archive(&id, root.as_deref()),
        },
        Some(Commands::Memory { action }) => match action {
            MemoryAction::Search { query, user, top_k } => {
                cmd_memory::cmd_memory_search(&query, &user, top_k);
            }
            MemoryAction::List { user, session } => {
                cmd_memory::cmd_memory_list(&user, session.as_deref());
            }
        },
        Some(Commands::Embed { action }) => match action {
            EmbedAction::Generate {
                text,
                model,
                service,
                api_key,
            } => {
                cmd_embed::cmd_embed_generate(
                    &text,
                    model.as_deref(),
                    service.as_deref(),
                    api_key.as_deref(),
                );
            }
            EmbedAction::Batch {
                texts,
                model,
                service,
                api_key,
            } => {
                cmd_embed::cmd_embed_batch(
                    &texts,
                    model.as_deref(),
                    service.as_deref(),
                    api_key.as_deref(),
                );
            }
            EmbedAction::List => cmd_embed::cmd_embed_list(),
            EmbedAction::Health {
                model,
                service,
                api_key,
            } => {
                cmd_embed::cmd_embed_health(
                    model.as_deref(),
                    service.as_deref(),
                    api_key.as_deref(),
                );
            }
            EmbedAction::Dimension {
                model,
                service,
                api_key,
            } => {
                cmd_embed::cmd_embed_dimension(
                    model.as_deref(),
                    service.as_deref(),
                    api_key.as_deref(),
                );
            }
        },
        Some(Commands::Usage { action }) => match action {
            UsageAction::Report => cmd_usage::cmd_usage_report(),
            UsageAction::Session { id } => cmd_usage::cmd_usage_session(&id),
            UsageAction::Export { format } => cmd_usage::cmd_usage_export(&format),
        },
        Some(Commands::Provider { action }) => match action {
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
        },
        Some(Commands::Token { action }) => match action {
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
            TokenAction::Catalog => {
                cmd_token::run_token_catalog();
            }
            TokenAction::Compat { base_url } => {
                cmd_token::run_token_compat(&base_url);
            }
            TokenAction::Fits { text, model } => {
                cmd_token::run_token_fits(&text, &model);
            }
            TokenAction::Probe { model } => {
                let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                rt.block_on(cmd_token::run_token_probe(model.as_deref(), &config));
            }
        },
        Some(Commands::Workflow { action }) => match action {
            WorkflowAction::List { domain } => {
                cmd_workflow::cmd_workflow_list(&config, domain.as_deref());
            }
            WorkflowAction::Show { name, domain } => {
                cmd_workflow::cmd_workflow_show(&name, &config, domain.as_deref());
            }
            WorkflowAction::Run {
                name,
                task,
                domain,
                model,
                user,
            } => {
                cmd_workflow::cmd_workflow_run(
                    &name,
                    task.as_deref(),
                    &config,
                    domain.as_deref(),
                    model.as_deref(),
                    user.as_deref(),
                );
            }
        },
        Some(Commands::Graph { action }) => match action {
            GraphAction::List { domain } => {
                cmd_workflow::cmd_graph_list(&config, domain.as_deref());
            }
            GraphAction::Show { name, domain } => {
                cmd_workflow::cmd_graph_show(&name, &config, domain.as_deref());
            }
            GraphAction::Run {
                name,
                task,
                domain,
                model,
                user,
            } => {
                cmd_workflow::cmd_graph_run(
                    &name,
                    &task,
                    &config,
                    domain.as_deref(),
                    model.as_deref(),
                    user.as_deref(),
                );
            }
        },
        Some(Commands::Version) => {
            cmd_version::cmd_version();
        }
        Some(Commands::Init {
            format,
            path,
            force,
            no_llm,
        }) => {
            cmd_init::cmd_init(&config, Some(&format), path.as_deref(), force, no_llm);
        }
    }
}
