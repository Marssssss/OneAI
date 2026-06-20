//! CLI provider subcommand — multi-provider fallback pool management and smart routing.

use oneai_core::ProviderPoolConfig;
use oneai_core::SmartRouteConfig;
use oneai_core::RoutingStrategy;
use oneai_core::ModelPricingCatalog;
use oneai_provider::ModelRouter;
use oneai_provider::SmartRouter;
use oneai_core::ModelConfig;
use std::collections::HashMap;

/// Show provider pool status — active provider, health, circuit states.
pub fn run_provider_status() -> i32 {
    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Provider Pool Status               ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    // Check if we can read pool config from environment
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let openai_key = std::env::var("OPENAI_API_KEY").ok();

    if anthropic_key.is_none() && openai_key.is_none() {
        println!("  ⚠  No API keys configured (ANTHROPIC_API_KEY, OPENAI_API_KEY)");
        println!("  Set environment variables or use provider_pool_config() in code.");
        println!();
        println!("  Available providers (local):");
        println!("    • Ollama — http://localhost:11434 (if running)");
        return 0;
    }

    // Build pool config from available keys
    let config = if anthropic_key.is_some() {
        ProviderPoolConfig::anthropic_primary(anthropic_key.clone(), openai_key.clone())
    } else {
        ProviderPoolConfig::openai_primary(openai_key.clone(), anthropic_key.clone())
    };

    println!("  Pool Configuration:");
    println!("    • Max fallbacks: {}", config.max_fallbacks);
    println!("    • Degradation enabled: {}", config.degrade_on_fallback);
    println!("    • Provider entries: {}", config.entry_count());
    println!();

    let sorted = config.sorted_entries();
    println!("  Provider Chain (priority order):");
    for entry in sorted {
        let key_status = if entry.model_config.api_key.is_some() {
            "✓ key set"
        } else {
            "✗ no key"
        };
        println!("    • {} — {} (priority {}, cooldown {}s) [{}]",
            entry.name,
            entry.model_name(),
            entry.priority,
            entry.cooldown_secs,
            key_status,
        );
    }

    println!();

    if config.degrade_on_fallback {
        println!("  Model Degradation Rules:");
        for rule in &config.degradation_rules {
            println!("    • {} family: {}", rule.provider_family, rule.chain.join(" → "));
        }
        println!();
    }

    println!("  💡 To test actual provider connectivity, use: oneai provider test");
    0
}

/// Show recent fallback events from the pool log.
pub fn run_fallback_log_with_limit(limit: &str) -> i32 {
    let _limit: usize = limit.parse().unwrap_or(20);

    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Provider Fallback Log               ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    println!("  Fallback events are logged when a provider fails and the pool");
    println!("  automatically switches to an alternative provider.");
    println!();

    println!("  Recent events (last {}):", _limit);
    println!("  ─────────────────────────────────────────────────────");
    println!("  No events recorded (pool not active in current session).");
    println!();
    println!("  💡 Fallback events are recorded during active AgentLoop sessions.");
    println!("  Run `oneai run <task>` to start a session and generate fallback data.");

    0
}

/// Test all providers in the pool with a connectivity check.
pub fn run_provider_test() -> i32 {
    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Provider Connectivity Test           ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let openai_key = std::env::var("OPENAI_API_KEY").ok();

    println!("  Testing provider connectivity...");
    println!();

    // Test Anthropic
    if let Some(key) = &anthropic_key {
        println!("  • Anthropic (claude-haiku-4-5-20251001):");
        let display_key = if key.len() > 12 {
            format!("{}...{}", &key[..8], &key[key.len()-4..])
        } else {
            format!("{}...{}", &key[..4.min(key.len())], if key.len() > 4 { &key[key.len()-4.min(key.len())..] } else { "" })
        };
        println!("    Key: {}", display_key);
        println!("    Status: Would need real API call — use `oneai run` for live testing");
    } else {
        println!("  • Anthropic: ✗ No ANTHROPIC_API_KEY set");
    }

    println!();

    // Test OpenAI
    if let Some(key) = &openai_key {
        println!("  • OpenAI (gpt-4o-mini):");
        let display_key = if key.len() > 12 {
            format!("{}...{}", &key[..8], &key[key.len()-4..])
        } else {
            format!("{}...{}", &key[..4.min(key.len())], if key.len() > 4 { &key[key.len()-4.min(key.len())..] } else { "" })
        };
        println!("    Key: {}", display_key);
        println!("    Status: Would need real API call — use `oneai run` for live testing");
    } else {
        println!("  • OpenAI: ✗ No OPENAI_API_KEY set");
    }

    println!();

    // Test Ollama
    println!("  • Ollama (localhost:11434):");
    println!("    Status: Check if server is running with: curl http://localhost:11434/api/tags");

    println!();
    println!("  💡 For live provider testing, run: oneai run \"ping test\"");
    0
}

/// Show routing decision for a task (dry run) — cost/latency/quality analysis.
pub fn run_route_dry_run(task: &str, strategy: &str) -> i32 {
    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Smart Routing Decision              ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    // Parse strategy
    let routing_strategy = match strategy.to_lowercase().as_str() {
        "cost" | "cost-optimized" => RoutingStrategy::CostOptimized,
        "latency" | "latency-optimized" => RoutingStrategy::LatencyOptimized,
        "quality" | "quality-optimized" => RoutingStrategy::QualityOptimized,
        "balanced" => RoutingStrategy::Balanced,
        _ => {
            println!("  ⚠ Unknown strategy '{}', using 'balanced'", strategy);
            RoutingStrategy::Balanced
        },
    };

    println!("  Task: \"{}\"", task);
    println!("  Strategy: {} (weights: cost={}, latency={}, quality={})",
        routing_strategy.name(),
        routing_strategy.weights().0,
        routing_strategy.weights().1,
        routing_strategy.weights().2,
    );
    println!();

    // Build smart router config
    let smart_config = SmartRouteConfig::with_strategy(routing_strategy);

    // Build a default ModelRouter for the first-pass regex evaluation
    let fallback_config = ModelConfig {
        provider_type: oneai_core::ProviderType::Cloud,
        cloud_kind: Some(oneai_core::CloudProviderKind::Anthropic),
        api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
        base_url: None,
        port: None,
        model_name: Some("claude-sonnet-4-6-20250514".to_string()),
        model_path: None,
        extra: HashMap::new(),
    };
    let model_router = ModelRouter::with_defaults(fallback_config);
    let catalog = ModelPricingCatalog::with_known_models();

    // Run the smart router (no budget/health/rate constraints in dry run)
    let router = SmartRouter::new(
        model_router,
        catalog,
        smart_config.without_budget_awareness().without_health_awareness().without_rate_awareness(),
    );

    // Use tokio runtime for async routing
    let rt = tokio::runtime::Runtime::new().unwrap();
    let decision = rt.block_on(router.route(task, "react", None, None));

    println!("  Routing Decision:");
    println!("  ─────────────────────────────────────────────────────");
    println!("  Model:        {} ({})", decision.model, decision.tier.name());
    println!("  Provider:     {}", decision.provider);
    println!("  Source:       {}", if decision.from_regex { "regex rule" } else { "multi-factor scoring" });
    println!("  Quality:      {:.2}", decision.quality_score);
    println!("  Total Score:  {:.2}", decision.total_score);
    if decision.estimated_cost_usd > 0.0 {
        println!("  Est. Cost:    ${:.4}", decision.estimated_cost_usd);
    }
    if decision.estimated_latency_ms > 0 {
        println!("  Est. Latency: ~{}ms", decision.estimated_latency_ms);
    }
    println!();

    // Show factor analysis
    println!("  Factors Considered:");
    for factor in &decision.factors {
        println!("    • {}", factor.description());
    }
    println!();

    // Show all provider scores
    if !decision.all_scores.is_empty() {
        println!("  All Provider Scores:");
        println!("  ─────────────────────────────────────────────────────");
        for score in &decision.all_scores {
            println!("    {}", score.summary());
        }
        println!();
    }

    println!("  💡 To enable budget/health/rate constraints, use: oneai run <task>");
    0
}

/// Show recent routing decisions from the routing log.
pub fn run_route_log(limit: &str) -> i32 {
    let _limit: usize = limit.parse().unwrap_or(10);

    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Smart Routing Log                    ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    println!("  Routing decisions are logged during active AgentLoop sessions.");
    println!("  Each decision includes model, provider, strategy, and rationale.");
    println!();

    println!("  Recent decisions (last {}):", _limit);
    println!("  ─────────────────────────────────────────────────────");
    println!("  No decisions recorded (router not active in current session).");
    println!();
    println!("  💡 Run `oneai run <task>` to start a session and generate routing data.");

    0
}

/// Show current routing strategy and configuration.
pub fn run_route_config() -> i32 {
    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Smart Routing Configuration          ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    println!("  Available Strategies:");
    println!("  ─────────────────────────────────────────────────────");
    println!("    • Balanced (default)     — cost=0.30, latency=0.30, quality=0.40");
    println!("    • Cost Optimized         — cost=0.70, latency=0.10, quality=0.20");
    println!("    • Latency Optimized      — cost=0.10, latency=0.70, quality=0.20");
    println!("    • Quality Optimized      — cost=0.10, latency=0.10, quality=0.80");
    println!("    • Custom                 — user-defined weights");
    println!();

    println!("  Routing Model Tiers:");
    println!("  ─────────────────────────────────────────────────────");
    println!("    • Cheap    (quality=0.30) — Haiku, gpt-4o-mini, Gemini Flash, Ollama small");
    println!("    • Balanced (quality=0.70) — Sonnet, gpt-4o, Gemini 2.5-flash, Ollama 7b");
    println!("    • Powerful (quality=1.00) — Opus, o3-pro, Gemini Pro, deepseek-r1:14b");
    println!();

    println!("  Runtime Constraints (when enabled):");
    println!("  ─────────────────────────────────────────────────────");
    println!("    • Budget awareness    — skip expensive models when budget is low");
    println!("    • Health awareness    — skip providers with open circuit breakers");
    println!("    • Rate awareness      — skip providers that are rate-limited");
    println!("    • Context awareness   — skip models whose context window would overflow");
    println!("    • Regex first-pass    — try keyword rules before multi-factor scoring");
    println!();

    println!("  💡 Configure in code:");
    println!("    AppBuilder::new()");
    println!("      .default_provider_pool_anthropic()");
    println!("      .default_smart_router_cost_optimized()  // or balanced/latency/quality");
    println!("      .build()");

    0
}
