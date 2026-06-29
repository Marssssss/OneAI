//! Handoff protocol CLI commands.
//!
//! Provides CLI subcommands for managing and running agent handoffs:
//!   oneai handoff list      — List available handoff targets
//!   oneai handoff targets   — Show detailed handoff target descriptions
//!   oneai handoff config    — Show current handoff configuration
//!   oneai handoff run <t> <r> — Execute a handoff (demo mode)

use oneai_core::handoff::{
    HandoffConfig, HandoffPresets, HandoffResult,
};

/// List available handoff targets with brief descriptions.
pub fn cmd_handoff_list() {
    println!("Available Handoff Targets:\n");

    let configs = [
        ("development_chain", "Development Handoff Chain", "3 targets (coding, review, research) — conversation transfer"),
        ("research_chain", "Research Handoff Chain", "2 targets (research, analysis) — conversation transfer"),
        ("support_routing", "Support Routing Handoff", "3 targets (specialists) — summary-only transfer"),
    ];

    for (id, name, desc) in &configs {
        println!("  {} — {}", id, name);
        println!("    {}", desc);
        println!();
    }

    println!("Usage: oneai handoff targets <preset>");
    println!("       oneai handoff run <target> <reason>");
}

/// Show detailed handoff target descriptions for a preset.
pub fn cmd_handoff_targets(preset: &str) {
    let config = resolve_handoff_config(preset);
    match config {
        Some(config) => {
            println!("Handoff Targets for '{}':\n", preset);
            for target in &config.targets {
                println!("  {} ({})", target.agent_name, target.agent_kind);
                println!("    Description: {}", target.description);
                println!("    Can hand off: {}", target.can_handoff);
                if let Some(prompt) = &target.system_prompt_override {
                    println!("    System prompt: {}...", prompt.chars().take(80).collect::<String>());
                }
                println!();
            }

            println!("Configuration:");
            println!("  Transfer conversation: {}", config.transfer_conversation);
            println!("  Max depth: {}", config.max_depth);
            println!("  Tool name: {}", config.tool_name);
        }
        None => {
            println!("Unknown preset: '{}'. Available presets: development_chain, research_chain, support_routing", preset);
        }
    }
}

/// Show current handoff configuration details.
pub fn cmd_handoff_config(preset: Option<&str>) {
    let config = preset
        .and_then(resolve_handoff_config)
        .or_else(|| resolve_handoff_config("development_chain"));

    match config {
        Some(config) => {
            println!("Handoff Configuration:\n");
            println!("  Targets: {} ({})", config.target_count(), config.target_names().join(", "));
            println!("  Transfer conversation: {}", config.transfer_conversation);
            println!("  Max depth: {}", config.max_depth);
            println!("  Add handoff message: {}", config.add_handoff_message);
            println!("  Tool name: {}", config.tool_name);

            // Show tool description
            println!("\nTool Description (shown to model):");
            println!("{}", config.tool_description());

            // Validate
            match config.validate() {
                Ok(_) => println!("\n✓ Configuration is valid"),
                Err(e) => println!("\n✗ Configuration error: {}", e),
            }
        }
        None => {
            println!("No handoff configuration available. Use --preset to specify one.");
        }
    }
}

/// Execute a handoff (demo mode).
pub fn cmd_handoff_run(target: &str, reason: &str, preset: Option<&str>) {
    let config = preset
        .and_then(resolve_handoff_config)
        .or_else(|| resolve_handoff_config("development_chain"));

    match config {
        Some(config) => {
            // Validate target
            if config.target_by_name(target).is_none() {
                println!("✗ Unknown handoff target '{}'. Available targets: {}", target, config.target_names().join(", "));
                return;
            }

            println!("Executing handoff:\n");
            println!("  Target: {}", target);
            println!("  Reason: {}", reason);
            println!("  Transfer conversation: {}", config.transfer_conversation);
            println!("  Max depth: {}", config.max_depth);

            let target_info = config.target_by_name(target).unwrap();
            println!("  Agent kind: {}", target_info.agent_kind);
            println!("  Can hand off further: {}", target_info.can_handoff);

            // Note: In a full implementation, this would create a real HandoffManager
            // with a DefaultSubAgentFactory wired through AppBuilder.
            println!("\n⚠ Handoff execution requires a configured LLM provider.");
            println!("   Configure provider via: oneai config init");
            println!("   Then use: oneai handoff run {} \"{}\"", target, reason);
        }
        None => {
            println!("No handoff configuration available. Use --preset to specify one.");
        }
    }
}

/// Resolve a handoff config from a preset name.
fn resolve_handoff_config(name: &str) -> Option<HandoffConfig> {
    match name {
        "development_chain" | "development" => Some(HandoffPresets::development_chain()),
        "research_chain" | "research" => Some(HandoffPresets::research_chain()),
        "support_routing" | "support" => Some(HandoffPresets::support_routing()),
        _ => None,
    }
}

/// Format a handoff result for display.
#[allow(dead_code)]
pub fn format_handoff_result(result: &HandoffResult) -> String {
    let mut output = String::new();

    output.push_str("Handoff Result\n");
    output.push_str(&format!("Final Answer:\n{}\n\n", result.final_answer));

    if result.has_handoffs() {
        output.push_str("Handoff Chain:\n");
        for entry in &result.chain {
            output.push_str(&format!(
                "  {} → {} (reason: {}, {} tokens)\n",
                entry.from_agent, entry.to_agent, entry.reason, entry.tokens_used
            ));
        }
        output.push_str(&format!(
            "\nTotal: {} handoffs, {} tokens\n",
            result.handoff_count, result.total_tokens
        ));
    } else {
        output.push_str("No handoffs occurred — single agent handled the task.\n");
    }

    output
}
