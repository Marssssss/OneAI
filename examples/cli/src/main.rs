//! OneAI CLI — interactive REPL and non-interactive inference.
//!
//! Subcommands:
//!   oneai chat          — Launch the interactive TUI
//!   oneai run <prompt>  — Single-shot inference (stdout)
//!   oneai studio        — Launch Studio Web UI (port 3000)
//!   oneai pack list     — List available DomainPacks
//!   oneai pack show <n> — Show DomainPack details
//!   oneai pack install  — Install a DomainPack
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
        Some(Commands::Version) => {
            cmd_version::cmd_version();
        }
    }
}
