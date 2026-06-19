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
//!   oneai cost report          — Show global cost summary
//!   oneai cost session <id>    — Show per-session cost details
//!   oneai cost budget <max>    — Set session budget limit (USD)
//!   oneai cost models          — List pricing for known models
//!   oneai cost export [--format]— Export usage records (json/csv)
//!   oneai provider status      — Show provider pool status and health
//!   oneai provider fallback-log — Show recent fallback events
//!   oneai provider test        — Test all providers connectivity
//!   oneai eval list     — List available eval suites
//!   oneai eval run <n>  — Run an eval suite
//!   oneai eval score <n>— Run metrics only (no agent)
//!   oneai config show   — Show current configuration
//!   oneai config init   — Create default config file
//!   oneai version       — Version information

mod config;
mod cmd_chat;
mod cmd_run;
mod cmd_pack;
mod cmd_eval;
mod cmd_config;
mod cmd_version;
mod cmd_studio;
mod cmd_mcp;
mod cmd_a2a;
mod cmd_wasm;
mod cmd_session;
mod cmd_embed;
mod cmd_cost;
mod cmd_provider;
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
    },
    /// Launch Studio Web UI for visualizing agent execution
    Studio {
        /// Port to listen on (default: 3000)
        #[arg(long, default_value_t = 3000)]
        port: u16,
        /// Domain pack to use
        #[arg(long)]
        domain: Option<String>,
    },
    /// Manage domain packs
    Pack {
        #[command(subcommand)]
        action: PackAction,
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
    /// Embedding service — generate vector embeddings for text
    Embed {
        #[command(subcommand)]
        action: EmbedAction,
    },
    /// Cost & usage management — track LLM inference costs, budgets, and model pricing
    Cost {
        #[command(subcommand)]
        action: CostAction,
    },
    /// Provider pool management — multi-provider fallback status and health
    Provider {
        #[command(subcommand)]
        action: ProviderAction,
    },
    /// Show version information
    Version,
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
    },
    /// Run metrics only (no agent execution — uses expected answers as outputs)
    Score {
        /// Suite name
        name: String,
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
enum CostAction {
    /// Show global cost summary (total tokens, cost, by-model breakdown)
    Report,
    /// Show per-session cost details
    Session {
        /// Session ID to inspect
        id: String,
    },
    /// Set session budget limit in USD (e.g., 5.0 = $5 max)
    Budget {
        /// Maximum cost in USD (e.g., 5.0)
        max_usd: String,
    },
    /// List pricing for known models
    Models,
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

fn main() {
    let cli = Cli::parse();

    let config = config::OneaiConfig::load_or_default();

    match cli.command {
        None => {
            // Default: launch TUI (same as "oneai chat" with no options)
            cmd_chat::cmd_chat(&config, None, None);
        }
        Some(Commands::Chat { domain, model }) => {
            cmd_chat::cmd_chat(&config, domain.as_deref(), model.as_deref());
        }
        Some(Commands::Run { prompt, domain, model }) => {
            cmd_run::cmd_run(&prompt, &config, domain.as_deref(), model.as_deref());
        }
        Some(Commands::Studio { port, domain }) => {
            cmd_studio::cmd_studio(port, domain.as_deref());
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
        Some(Commands::Eval { action }) => {
            match action {
                EvalAction::List => cmd_eval::cmd_eval_list(),
                EvalAction::Run { name, format } => {
                    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                    rt.block_on(cmd_eval::cmd_eval_run(&name, &format));
                }
                EvalAction::Score { name } => {
                    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
                    rt.block_on(cmd_eval::cmd_eval_score(&name));
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
        Some(Commands::Cost { action }) => {
            match action {
                CostAction::Report => cmd_cost::cmd_cost_report(),
                CostAction::Session { id } => cmd_cost::cmd_cost_session(&id),
                CostAction::Budget { max_usd } => cmd_cost::cmd_cost_budget(&max_usd),
                CostAction::Models => cmd_cost::cmd_cost_models(),
                CostAction::Export { format } => cmd_cost::cmd_cost_export(&format),
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
        Some(Commands::Version) => {
            cmd_version::cmd_version();
        }
    }
}
