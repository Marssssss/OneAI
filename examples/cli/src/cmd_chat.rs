//! Chat command — launch the interactive TUI.
//!
//! This command starts the full Terminal User Interface (TUI) for interactive
//! agent conversations. It's the primary way to use OneAI interactively.

use std::sync::Arc;
use oneai_core::ModelConfig;
use oneai_app::AppBuilder;
use oneai_tool::CalculatorTool;
use oneai_domain::{DomainPack, coding_pack, research_pack};

use crate::config::OneaiConfig;
use crate::cmd_pack::get_builtin_pack;

/// Launch the TUI chat interface.
///
/// Steps:
/// 1. Load config → build ModelConfig
/// 2. Build App with provider + DomainPack + approval gate
/// 3. Register domain tools
/// 4. Launch TUI (delegates to tui::run_tui)
pub fn cmd_chat(config: &OneaiConfig, domain_override: Option<&str>, model_override: Option<&str>) {
    tracing_subscriber::fmt::init();

    // Check if we're running in a TTY (interactive terminal)
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        eprintln!("Error: OneAI TUI requires an interactive terminal (TTY).");
        eprintln!("Please run this in a terminal, not in a pipe or script.");
        eprintln!("For non-interactive usage, use: oneai run <prompt>");
        std::process::exit(1);
    }

    // Build ModelConfig from config + CLI overrides
    let provider_config = config.to_model_config_with_overrides(model_override);

    // Determine domain pack (from CLI override, config, or default)
    let domain_name = config.default_domain_pack(domain_override);

    // Launch TUI
    if let Err(e) = crate::tui::run_tui(provider_config, Some(&domain_name)) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
