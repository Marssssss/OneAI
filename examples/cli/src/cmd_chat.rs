//! Chat command — launch the interactive TUI.
//!
//! This command starts the full Terminal User Interface (TUI) for interactive
//! agent conversations. It's the primary way to use OneAI interactively.


use crate::config::OneaiConfig;

/// Launch the TUI chat interface.
///
/// Steps:
/// 1. Load config → build ModelConfig
/// 2. Build App with provider + DomainPack + approval gate
/// 3. Register domain tools
/// 4. Launch TUI (delegates to tui::run_tui)
pub fn cmd_chat(config: &OneaiConfig, domain_override: Option<&str>, model_override: Option<&str>) {
    // Initialize tracing to a log file — since the TUI takes over the terminal,
    // stderr/stdout logs can't be read from the TUI. Instead, all tracing output
    // goes to ~/.oneai/logs/oneai.log (with rolling file appender).
    init_file_logging();

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

/// Initialize file-based logging for the TUI.
///
/// The TUI occupies the entire terminal, so stderr/stdout logging is invisible.
/// Instead, we write all tracing output to a log file that can be inspected
/// after the session. The log file is at ~/.oneai/logs/oneai.log.
///
/// Log level can be controlled via RUST_LOG environment variable (default: info).
fn init_file_logging() {
    use std::path::PathBuf;
    use tracing_subscriber::EnvFilter;

    // Create ~/.oneai/logs/ directory if it doesn't exist
    let log_dir: PathBuf = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".oneai")
        .join("logs");

    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        // If we can't create the log directory, fall back to stderr
        // (still visible if the TUI crashes before taking over the terminal)
        eprintln!("Warning: couldn't create log directory {:?}: {}. Using stderr fallback.", log_dir, e);
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env()
                .add_directive("oneai_agent=info".parse().unwrap())
                .add_directive("oneai_provider=info".parse().unwrap()))
            .init();
        return;
    }

    let log_file_path = log_dir.join("oneai.log");

    // Try to open the log file
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path);

    match log_file {
        Ok(file) => {
            // Write a session separator line so each run is easy to find
            use std::io::Write;
            let _ = writeln!(&file, "\n═══════════════════════════════════════════════════════════════");
            let _ = writeln!(&file, "OneAI session started at {}", chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"));
            let _ = writeln!(&file, "═══════════════════════════════════════════════════════════════\n");

            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env()
                    .add_directive("oneai_agent=info".parse().unwrap())
                    .add_directive("oneai_provider=info".parse().unwrap()))
                .with_writer(file)
                .with_ansi(false) // No ANSI colors in log file
                .init();

            eprintln!("📋 Log file: {}", log_file_path.display());
            eprintln!("   (Logs are written to this file since TUI occupies the terminal)");
        }
        Err(e) => {
            eprintln!("Warning: couldn't open log file {:?}: {}. Using stderr fallback.", log_file_path, e);
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env()
                    .add_directive("oneai_agent=info".parse().unwrap())
                .add_directive("oneai_provider=info".parse().unwrap()))
                .init();
        }
    }
}
