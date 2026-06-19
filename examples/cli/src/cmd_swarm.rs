//! Swarm orchestration CLI commands.
//!
//! Provides CLI subcommands for managing and running agent swarms:
//!   oneai swarm list      — List available swarm presets
//!   oneai swarm routing   — Show routing strategies
//!   oneai swarm config <p>— Show swarm configuration details
//!   oneai swarm agents <p>— Show agents and capabilities in a preset
//!   oneai swarm run <task>— Execute a swarm task

use oneai_core::swarm::{
    SwarmConfig, SwarmPresets, SwarmRouting, SwarmResult,
};

#[allow(dead_code)]
use oneai_core::swarm::SwarmTaskResult;

/// List available swarm presets with brief descriptions.
pub fn cmd_swarm_list() {
    println!("Available Swarm Presets:\n");

    let presets = [
        ("code_analysis", "Code Analysis Swarm", "BestFit routing, 4 agents (coder, researcher, reviewer, planner)"),
        ("fast_research", "Fast Research Swarm", "Fastest routing, 3 agents (researcher, planner, reviewer)"),
        ("budget_code", "Budget Code Swarm", "CostOptimized routing, 4 agents — cheapest quality-acceptable agent"),
        ("balanced_dev", "Balanced Dev Swarm", "LoadBalanced routing, 4 agents — distributes evenly"),
    ];

    for (id, name, desc) in &presets {
        println!("  {} — {}", id, name);
        println!("    {}", desc);
        println!();
    }

    println!("Routing strategies: best-fit, load-balanced, cost-optimized, fastest");
    println!("Usage: oneai swarm run <task> [--routing best-fit] [--preset code_analysis]");
}

/// Show available routing strategies with descriptions.
pub fn cmd_swarm_routing() {
    println!("Swarm Routing Strategies:\n");

    for strategy in SwarmRouting::all() {
        println!("  {} — {}", strategy.name(), strategy.description());
        println!();
    }

    println!("Each strategy optimizes for a different goal when assigning tasks to agents.");
    println!("The swarm router considers agent capabilities (quality, speed, cost) when routing.");
}

/// Show swarm configuration details for a preset.
pub fn cmd_swarm_config(preset: &str) {
    let config = resolve_swarm_config(preset);
    match config {
        Some(config) => {
            println!("Swarm Configuration for '{}':\n", preset);
            println!("  ID: {}", config.id);
            println!("  Routing: {} ({})", config.routing.name(), config.routing.description());
            println!("  Quality threshold: {}", config.quality_threshold);
            println!("  Max retries: {}", config.max_retries);
            println!("  Budget: {}", config.budget);
            println!("  Max concurrent tasks: {}", config.max_concurrent_tasks);
            println!("  Agents: {} ({})", config.agent_count(), config.agent_names().join(", "));
            println!("  Categories: {}", config.all_categories().join(", "));

            // Validate
            match config.validate() {
                Ok(_) => println!("\n✓ Configuration is valid"),
                Err(e) => println!("\n✗ Configuration error: {}", e),
            }
        }
        None => {
            println!("Unknown preset: '{}'. Available presets: code_analysis, fast_research, budget_code, balanced_dev", preset);
        }
    }
}

/// Show agents and capabilities in a swarm preset.
pub fn cmd_swarm_agents(preset: &str) {
    let config = resolve_swarm_config(preset);
    match config {
        Some(config) => {
            println!("Swarm Agents for '{}':\n", preset);

            for agent in &config.agents {
                println!("  {} ({})", agent.name, agent.agent_kind);
                println!("    Categories: {}", agent.capability.categories.join(", "));
                println!("    Quality scores: {}", format_scores(&agent.capability.quality_scores));
                println!("    Speed scores: {}", format_scores(&agent.capability.speed_scores));
                println!("    Max concurrent: {}", agent.capability.max_concurrent);
                println!("    Cost per 1k tokens: ${:.3}", agent.capability.cost_per_1k);
                if let Some(prompt) = &agent.system_prompt_override {
                    println!("    System prompt: {}...", prompt.chars().take(80).collect::<String>());
                }
                println!();
            }

            println!("Routing: {}", config.routing.description());
            println!("Quality threshold: {}", config.quality_threshold);
        }
        None => {
            println!("Unknown preset: '{}'. Available presets: code_analysis, fast_research, budget_code, balanced_dev", preset);
        }
    }
}

/// Execute a swarm task (demo mode).
pub fn cmd_swarm_run(task: &str, routing: &str, preset: Option<&str>, budget: Option<&str>) {
    let config = preset
        .and_then(resolve_swarm_config)
        .or_else(|| resolve_swarm_config("code_analysis"));

    // Apply routing override
    let config = config.map(|c| {
        if let Some(routing_strategy) = SwarmRouting::from_str_opt(routing) {
            c.with_routing(routing_strategy)
        } else {
            c
        }
    });

    // Apply budget override
    let config = config.map(|c| {
        if let Some(budget_str) = budget {
            if let Ok(tokens) = budget_str.parse::<u32>() {
                c.with_budget(oneai_core::team::TokenBudgetProxy::new(tokens))
            } else {
                c
            }
        } else {
            c
        }
    });

    match config {
        Some(config) => {
            println!("Executing swarm:\n");
            println!("  Task: {}", task);
            println!("  Routing: {}", config.routing.name());
            println!("  Agents: {} ({})", config.agent_count(), config.agent_names().join(", "));
            println!("  Categories: {}", config.all_categories().join(", "));
            println!("  Quality threshold: {}", config.quality_threshold);
            println!("  Budget: {}", config.budget);

            // Note: In a full implementation, this would create a real SwarmOrchestrator
            // with a DefaultSubAgentFactory wired through AppBuilder.
            println!("\n⚠ Swarm execution requires a configured LLM provider.");
            println!("   Configure provider via: oneai config init");
            println!("   Then use: oneai swarm run \"{}\" --routing {}", task, routing);
        }
        None => {
            println!("No swarm configuration available. Use --preset to specify one.");
        }
    }
}

/// Resolve a swarm config from a preset name.
fn resolve_swarm_config(name: &str) -> Option<SwarmConfig> {
    match name {
        "code_analysis" | "analysis" => Some(SwarmPresets::code_analysis_swarm()),
        "fast_research" | "research" => Some(SwarmPresets::fast_research_swarm()),
        "budget_code" | "budget" => Some(SwarmPresets::budget_code_swarm()),
        "balanced_dev" | "balanced" => Some(SwarmPresets::balanced_dev_swarm()),
        _ => None,
    }
}

/// Format a HashMap of scores for display.
fn format_scores(scores: &std::collections::HashMap<String, f64>) -> String {
    let entries: Vec<String> = scores.iter()
        .map(|(k, v)| format!("{}={:.2}", k, v))
        .collect();
    entries.join(", ")
}

/// Format a swarm result for display.
#[allow(dead_code)]
pub fn format_swarm_result(result: &SwarmResult) -> String {
    let mut output = String::new();

    output.push_str("Swarm Result\n");
    output.push_str(&format!("Final Answer:\n{}\n\n", result.final_answer));

    if result.has_successful_results() {
        output.push_str("Task Results:\n");
        for task in &result.task_results {
            output.push_str(&format!(
                "  [{}] → {} (quality: {:.2}, {} tokens, {} retries)\n",
                task.category, task.agent_name, task.quality_score, task.tokens_used, task.retry_count
            ));
            output.push_str(&format!("    {}\n", task.result_text.chars().take(100).collect::<String>()));
        }
        output.push_str(&format!(
            "\nActive agents: {}\nTotal: {} tokens, ${:.4} cost\n",
            result.active_agents.join(", "), result.total_tokens, result.total_cost
        ));
    } else {
        output.push_str("No tasks completed successfully.\n");
    }

    output
}
