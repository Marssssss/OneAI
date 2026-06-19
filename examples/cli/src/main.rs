//! OneAI CLI — interactive REPL and non-interactive inference.
//!
//! Subcommands:
//!   oneai chat          — Launch the interactive TUI
//!   oneai run <prompt>  — Single-shot inference (stdout)
//!   oneai studio        — Launch Studio Web UI (port 3000)
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
        Some(Commands::Version) => {
            cmd_version::cmd_version();
        }
    }
}
