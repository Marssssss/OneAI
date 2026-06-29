//! Team coordination CLI commands.
//!
//! Provides CLI subcommands for managing and running multi-agent teams:
//!   oneai team strategies  — List available team strategies
//!   oneai team presets     — List preset team configurations
//!   oneai team info <id>   — Show team configuration details
//!   oneai team create      — Create a custom team configuration (interactive)
//!   oneai team run <task>  — Run a team task with a strategy

use oneai_core::team::{
    TeamStrategy, TeamConfig, TeamPresets, TeamResult,
    AgentRole, SubAgentKindProxy, TokenBudgetProxy,
};

/// List available team strategies with descriptions.
pub fn cmd_team_strategies() {
    println!("Available Team Strategies:\n");
    for strategy in TeamStrategy::all() {
        println!("  {} — {}", strategy.name(), strategy.description());
    }
    println!("\nUsage: oneai team run --strategy <name> \"<task>\"");
}

/// List preset team configurations.
pub fn cmd_team_presets() {
    println!("Preset Team Configurations:\n");

    let presets = [
        ("code_review", "Code Review Team", "3 agents (security, style, correctness) — Coordinate strategy"),
        ("research_route", "Research Routing Team", "Router + 2 specialists — Route strategy"),
        ("dev_pipeline", "Development Pipeline Team", "3 stages (research → plan → code) — Collaborate strategy"),
        ("arch_debate", "Architecture Debate Team", "3 advocates + 1 judge — Debate strategy"),
    ];

    for (id, name, desc) in &presets {
        println!("  {} — {}", id, name);
        println!("    {}", desc);
        println!();
    }

    println!("Usage: oneai team run --preset <id> \"<task>\"");
}

/// Show details of a team configuration.
pub fn cmd_team_info(team_id: &str) {
    let config = resolve_team_config(team_id);
    match config {
        Some(config) => {
            println!("Team: {}", config.id);
            println!("Strategy: {} — {}", config.strategy.name(), config.strategy.description());
            println!("Budget: {}", config.budget);
            println!("Max concurrent: {}", config.max_concurrent);
            println!("Use coordinator agent: {}", config.use_coordinator_agent);
            println!("Roles:");
            for role in &config.roles {
                println!("  {} ({})", role.name, role.agent_kind);
                println!("    Backstory: {}", role.backstory);
                println!("    Tools: {}", role.available_tools.join(", "));
            }

            // Validate
            match config.validate() {
                Ok(_) => println!("\n✓ Configuration is valid"),
                Err(e) => println!("\n✗ Configuration error: {}", e),
            }
        }
        None => {
            println!("Unknown team: '{}'. Available presets: code_review, research_route, dev_pipeline, arch_debate", team_id);
        }
    }
}

/// Run a team task.
pub fn cmd_team_run(
    task: &str,
    strategy: &str,
    preset: Option<&str>,
    budget: Option<&str>,
) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    rt.block_on(async {
        println!("Running team task: \"{}\"", task);

        // Resolve team config
        let config = if let Some(preset_name) = preset {
            resolve_team_config(preset_name)
        } else {
            // Create from strategy argument
            let strategy_kind = TeamStrategy::from_str_opt(strategy)
                .unwrap_or(TeamStrategy::Coordinate);
            Some(TeamConfig::new("cli_team", strategy_kind)
                .with_budget(TokenBudgetProxy::new(
                    budget.and_then(|b| b.parse::<u32>().ok()).unwrap_or(100_000)
                ))
                .with_role(AgentRole {
                    name: "explorer".into(),
                    backstory: "Codebase explorer".into(),
                    agent_kind: SubAgentKindProxy::explore(),
                    available_tools: vec!["read_file".into(), "grep".into()],
                    system_prompt_override: None,
                })
                .with_role(AgentRole {
                    name: "coder".into(),
                    backstory: "Code implementer".into(),
                    agent_kind: SubAgentKindProxy::code(),
                    available_tools: vec!["read_file".into(), "edit_file".into()],
                    system_prompt_override: None,
                })
                .with_role(AgentRole {
                    name: "reviewer".into(),
                    backstory: "Quality reviewer".into(),
                    agent_kind: SubAgentKindProxy::review(),
                    available_tools: vec!["read_file".into(), "grep".into()],
                    system_prompt_override: None,
                }))
        };

        match config {
            Some(config) => {
                // Validate first
                if let Err(e) = config.validate() {
                    println!("✗ Team configuration error: {}", e);
                    return;
                }

                println!("Team: {} (strategy: {}, {} roles)", config.id, config.strategy, config.role_count());

                // Note: In a full implementation, this would create a real TeamCoordinator
                // with a DefaultSubAgentFactory wired through AppBuilder.
                // For the CLI demo, we show the team configuration and would
                // run the coordinator if a provider is available.
                println!("\n⚠ Team execution requires a configured LLM provider.");
                println!("   Configure provider via: oneai config init");
                println!("   Then use: oneai team run --preset {} \"{}\"", config.id, task);

                // Print team roles
                println!("\nTeam roles:");
                for role in &config.roles {
                    println!("  • {} ({}) — {}", role.name, role.agent_kind, role.backstory);
                }
            }
            None => {
                println!("Could not resolve team configuration. Use --preset or --strategy.");
            }
        }
    });
}

/// Resolve a team config from a preset name or strategy.
fn resolve_team_config(name: &str) -> Option<TeamConfig> {
    match name {
        "code_review" => Some(TeamPresets::code_review_team()),
        "research_route" => Some(TeamPresets::research_routing_team()),
        "dev_pipeline" => Some(TeamPresets::development_pipeline_team()),
        "arch_debate" => Some(TeamPresets::architecture_debate_team()),
        other => {
            // Try to parse as a strategy
            let strategy = TeamStrategy::from_str_opt(other)?;
            Some(TeamConfig::new(other, strategy))
        }
    }
}

/// Format a team result for display.
#[allow(dead_code)]
pub fn format_team_result(result: &TeamResult) -> String {
    let mut output = String::new();

    output.push_str(&format!("Team Result (strategy: {})\n", result.strategy));
    output.push_str(&format!("Final Answer:\n{}\n\n", result.final_answer));

    output.push_str("Agent Results:\n");
    for entry in &result.agent_results {
        output.push_str(&format!(
            "  [{}] {} — {} tokens, completed: {}\n",
            entry.role, entry.agent_kind, entry.tokens_used, entry.completed
        ));
        if !entry.key_findings.is_empty() {
            for finding in &entry.key_findings {
                output.push_str(&format!("    • {}\n", finding));
            }
        }
    }

    output.push_str(&format!(
        "\nTotal: {} tokens\n",
        result.total_tokens
    ));

    output
}
