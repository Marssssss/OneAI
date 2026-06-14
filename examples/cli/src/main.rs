//! OneAI Interactive TUI — Terminal-based AI agent interface.
//!
//! This provides a rich terminal UI (TUI) inspired by opencode:
//! - Left sidebar with tools list and session info
//! - Right panel: header bar, scrollable chat area, input box at bottom
//! - Streaming/typewriter effect for assistant responses
//! - Enter=send, Ctrl+Enter=newline, Tab=toggle sidebar, Esc=vim, Ctrl+C=quit
//!
//! Usage:
//!   # No LLM — tool-only mode
//!   cargo run -p oneai-cli-demo
//!
//!   # 百炼 (阿里 DashScope)
//!   ONEAI_API_KEY=sk-xxx ONEAI_BASE_URL=https://dashscope.aliyuncs.com/compatible-mode/v1 \
//!     ONEAI_MODEL=qwen-plus cargo run -p oneai-cli-demo
//!
//!   # OpenAI
//!   ONEAI_API_KEY=sk-xxx ONEAI_MODEL=gpt-4 cargo run -p oneai-cli-demo
//!
//!   # DeepSeek
//!   ONEAI_API_KEY=sk-xxx ONEAI_BASE_URL=https://api.deepseek.com/v1 \
//!     ONEAI_MODEL=deepseek-chat cargo run -p oneai-cli-demo
//!
//!   # Ollama (local)
//!   ONEAI_BASE_URL=http://localhost:11434 ONEAI_MODEL=llama3 cargo run -p oneai-cli-demo

mod tui;

use oneai_core::ModelConfig;

/// Build ModelConfig from environment variables.
fn build_model_config_from_env() -> Option<ModelConfig> {
    let api_key = std::env::var("ONEAI_API_KEY").ok();
    let base_url = std::env::var("ONEAI_BASE_URL").ok();
    let model = std::env::var("ONEAI_MODEL").unwrap_or("gpt-4".to_string());

    if api_key.is_none() && base_url.is_none() {
        return None;
    }

    Some(ModelConfig {
        api_key,
        base_url,
        model_name: Some(model),
        ..ModelConfig::default()
    })
}

fn main() {
    tracing_subscriber::fmt::init();

    // Check if we're running in a TTY (interactive terminal)
    // If not, we can't enter raw mode / alternate screen, so show an error
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        eprintln!("Error: OneAI TUI requires an interactive terminal (TTY).");
        eprintln!("Please run this in a terminal, not in a pipe or script.");
        std::process::exit(1);
    }

    let provider_config = build_model_config_from_env();

    if let Err(e) = tui::run_tui(provider_config) {
        eprintln!("Error: {}", e);
    }
}
