//! CLI cost subcommand — cost tracking, budget management, and model pricing.


use oneai_core::cost::{CostTracker, InMemoryCostTracker, ModelPricingCatalog, CostBudgetConfig};

/// Show global cost summary.
pub fn cmd_cost_report() {
    let tracker = InMemoryCostTracker::new();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    let summary = rt.block_on(tracker.global_cost()).expect("Failed to get global cost");

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║               OneAI — Cost Report                       ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Total Calls:    {:>10}                              ║", summary.call_count);
    println!("║  Total Tokens:   {:>10}                              ║", summary.total_tokens);
    println!("║  Prompt Tokens:  {:>10}                              ║", summary.prompt_tokens);
    println!("║  Completion:     {:>10}                              ║", summary.completion_tokens);
    println!("║  Total Cost:     {:>10.4} USD                         ║", summary.total_cost_usd);
    println!("║  Avg Per Call:   {:>10.4} USD                         ║", summary.avg_cost_per_call);
    println!("╚══════════════════════════════════════════════════════════╝");

    if summary.call_count == 0 {
        println!();
        println!("  No inference calls recorded yet. Start a session to track costs.");
        println!();
        println!("  Tip: Use `oneai cost budget <max_usd>` to set session budget limits.");
    }
}

/// Show cost details for a specific session.
pub fn cmd_cost_session(id: &str) {
    let tracker = InMemoryCostTracker::new();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let session_cost = rt.block_on(tracker.session_cost(id)).expect("Failed to get session cost");
    let by_model = rt.block_on(tracker.cost_by_model(id)).expect("Failed to get per-model cost");

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║          OneAI — Session Cost Details                   ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Session ID:     {:<40}  ║", id);
    println!("║  Total Calls:    {:>10}                              ║", session_cost.call_count);
    println!("║  Total Cost:     {:>10.4} USD                         ║", session_cost.total_cost_usd);
    println!("╚══════════════════════════════════════════════════════════╝");

    if !by_model.is_empty() {
        println!();
        println!("  Cost by Model:");
        println!("  {:<25} {:>10} {:>10} {:>12}", "Model", "Calls", "Tokens", "Cost (USD)");
        println!("  {}", "─".repeat(60));
        for (model, summary) in &by_model {
            println!("  {:<25} {:>10} {:>10} {:>12.4}", model, summary.call_count, summary.total_tokens, summary.total_cost_usd);
        }
    } else {
        println!();
        println!("  No cost data found for session '{}'.", id);
        println!("  Tip: Use `oneai session list` to see available session IDs.");
    }
}

/// Set a session budget limit.
pub fn cmd_cost_budget(max_usd: &str) {
    let max: f64 = max_usd.parse().expect("Invalid budget value — must be a number (e.g., 5.0)");
    if max <= 0.0 {
        eprintln!("Budget must be positive (e.g., 5.0 for $5.00 USD limit)");
        return;
    }

    let _config = CostBudgetConfig::with_cost_limit(max);

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║          OneAI — Budget Configuration                   ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Budget Limit:   {:>10.2} USD                          ║", max);
    println!("║  When exceeded:  Agent loop terminates                  ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();
    println!("  To apply this budget, configure it in your AppBuilder:");
    println!("  ```rust");
    println!("  AppBuilder::new()");
    println!("      .cost_budget(CostBudgetConfig::with_cost_limit({}))", max);
    println!("      .build()");
    println!("  ```");
}

/// List pricing for known models.
pub fn cmd_cost_models() {
    let catalog = ModelPricingCatalog::with_known_models();

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║          OneAI — Known Model Pricing                    ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("  {:<25} {:<10} {:>12} {:>14}", "Model", "Provider", "Prompt/1K", "Completion/1K");
    println!("  {}", "─".repeat(65));

    for entry in catalog.pricing_entries() {
        let prompt_str = if entry.prompt_per_1k_usd == 0.0 {
            "FREE".to_string()
        } else {
            format!("${:.2}", entry.prompt_per_1k_usd)
        };
        let completion_str = if entry.completion_per_1k_usd == 0.0 {
            "FREE".to_string()
        } else {
            format!("${:.2}", entry.completion_per_1k_usd)
        };
        println!("  {:<25} {:<10} {:>12} {:>14}", entry.model_name, entry.provider, prompt_str, completion_str);
    }

    println!("╚══════════════════════════════════════════════════════════╝");
    println!();
    println!("  Prices are approximate and may change. Check provider websites for current rates.");
    println!("  {} models in catalog.", catalog.model_count());
}

/// Export usage records.
pub fn cmd_cost_export(format: &str) {
    let tracker = InMemoryCostTracker::new();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    let records = rt.block_on(tracker.global_records()).expect("Failed to get records");

    if records.is_empty() {
        println!("No usage records to export. Start a session first.");
        return;
    }

    match format {
        "json" => {
            let json = serde_json::to_string_pretty(&records).unwrap_or_default();
            println!("{}", json);
        }
        "csv" => {
            println!("session_id,model,provider,prompt_tokens,completion_tokens,cost_usd,timestamp");
            for record in &records {
                println!("{},{},{},{},{},{:.4},{}",
                    record.session_id,
                    record.model,
                    record.provider,
                    record.prompt_tokens,
                    record.completion_tokens,
                    record.cost_usd,
                    record.timestamp.to_rfc3339());
            }
        }
        _ => {
            eprintln!("Unknown format '{}'. Use 'json' or 'csv'.", format);
        }
    }
}
