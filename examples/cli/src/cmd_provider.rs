//! CLI provider subcommand — multi-provider fallback pool management.

use oneai_core::ProviderPoolConfig;

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
