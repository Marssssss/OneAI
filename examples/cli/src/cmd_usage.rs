//! CLI usage subcommand — token-usage tracking and export.
//!
//! OneAI tracks usage strictly by token dimensions (prompt / completion /
//! total / call_count / per-model). There is no USD cost or budget here.

use oneai_core::usage::{UsageTracker, InMemoryUsageTracker};

/// Show global usage summary.
pub fn cmd_usage_report() {
    let tracker = InMemoryUsageTracker::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let summary = rt.block_on(tracker.global_usage()).expect("Failed to get global usage");

    println!("╔════════════════════════════════════════════════════════╗");
    println!("║               OneAI — Usage Report                      ║");
    println!("╚════════════════════════════════════════════════════════╝");
    println!();
    println!("║  Total Tokens:   {:>12}                          ║", summary.total_tokens);
    println!("║  Prompt:         {:>12}                          ║", summary.prompt_tokens);
    println!("║  Completion:     {:>12}                          ║", summary.completion_tokens);
    println!("║  Total Calls:    {:>12}                          ║", summary.call_count);
    println!();

    if summary.call_count == 0 {
        println!("  No inference calls recorded yet. Start a session to track usage.");
        println!("  Tip: Use `oneai usage session <id>` to see per-session details.");
    }
}

/// Show usage details for a specific session.
pub fn cmd_usage_session(id: &str) {
    let tracker = InMemoryUsageTracker::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let session_usage = rt.block_on(tracker.session_usage(id)).expect("Failed to get session usage");
    let by_model = rt.block_on(tracker.usage_by_model(id)).expect("Failed to get per-model usage");

    println!("╔════════════════════════════════════════════════════════╗");
    println!("║          OneAI — Session Usage Details                  ║");
    println!("╚════════════════════════════════════════════════════════╝");
    println!();
    println!("║  Session ID:     {:>12}                          ║", id);
    println!("║  Total Calls:    {:>12}                          ║", session_usage.call_count);
    println!("║  Total Tokens:   {:>12}                          ║", session_usage.total_tokens);
    println!("║  Prompt:         {:>12}                          ║", session_usage.prompt_tokens);
    println!("║  Completion:     {:>12}                          ║", session_usage.completion_tokens);
    println!();

    if !by_model.is_empty() {
        println!("  Usage by Model:");
        println!("  {:<25} {:>10} {:>12} {:>12}", "Model", "Calls", "Tokens", "Prompt+Comp");
        for (model, summary) in &by_model {
            println!("  {:<25} {:>10} {:>12} {}+{}",
                model, summary.call_count, summary.total_tokens,
                summary.prompt_tokens, summary.completion_tokens);
        }
    } else {
        println!("  No usage data found for session '{}'.", id);
    }
}

/// Export usage records (JSON/CSV).
pub fn cmd_usage_export(format: &str) {
    let tracker = InMemoryUsageTracker::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let records = rt.block_on(tracker.global_records()).expect("Failed to get usage records");

    match format.to_lowercase().as_str() {
        "json" => {
            let json = serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".to_string());
            println!("{}", json);
        }
        "csv" => {
            println!("session_id,model,provider,prompt_tokens,completion_tokens,total_tokens,timestamp");
            for record in &records {
                println!("{},{},{},{},{},{},{}",
                    record.session_id, record.model, record.provider,
                    record.prompt_tokens, record.completion_tokens,
                    record.total_tokens(), record.timestamp);
            }
        }
        _ => {
            eprintln!("Unknown format '{}'. Use 'json' or 'csv'.", format);
        }
    }
}
