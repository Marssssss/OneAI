//! CLI token subcommand — token counting, context window profiles, and fit checking.

use oneai_core::{HeuristicTokenCounter, TokenCounter, ProviderTokenizerType};
use oneai_core::{ContextManager, ContextTrimmingStrategy};
use oneai_core::model_context::{builtin_lookup, BUILTIN_MODEL_CONTEXT, ModelContextResolver};
use std::sync::Arc;

/// Count tokens in a text string for a specific model.
pub fn run_token_count(text: &str, model: Option<&str>) -> i32 {
    let counter = HeuristicTokenCounter::new();
    let model_name = model.unwrap_or("default");

    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Token Count                         ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    let tokens = counter.count_tokens(text, model_name);
    let chars = text.chars().count();
    let language = oneai_core::token_counter::LanguageType::detect(text);

    println!("  Input: {} chars", chars);
    println!("  Model: {}", model_name);
    println!("  Language: {}", match language {
        oneai_core::token_counter::LanguageType::Latin => "Latin/English",
        oneai_core::token_counter::LanguageType::CJK => "CJK (Chinese/Japanese/Korean)",
        oneai_core::token_counter::LanguageType::Mixed => "Mixed",
    });
    println!("  Estimated tokens: {}", tokens);
    println!("  Chars per token: {:.1}", if tokens > 0 { chars as f64 / tokens as f64 } else { 0.0 });

    if let Some(model) = model {
        let context_window = counter.context_window_size(model);
        let utilization_pct = if context_window > 0 {
            tokens as f64 / context_window as f64 * 100.0
        } else {
            0.0
        };
        println!("  Context window: {}K tokens", context_window / 1000);
        println!("  Utilization: {:.1}%", utilization_pct);
    }

    println!();
    0
}

/// Show context window profile for a model.
pub fn run_token_context(model: &str) -> i32 {
    let counter = HeuristicTokenCounter::new();
    let profile = counter.profile_for_model(model);
    let tokenizer_type = ProviderTokenizerType::from_model_name(model);

    // Use the 3-layer resolver for the window number + source (no provider
    // attached here, so L2 probe is skipped — only L1/L3 layers are reachable).
    let resolver = ModelContextResolver::empty();
    let (context_window, source) = resolver.resolve_with_source(model);

    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Context Window Profile              ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    println!("  Model: {}", model);
    println!("  Tokenizer: {}", tokenizer_type.name());
    println!("  Context window: {}K tokens", context_window / 1000);
    println!("  Max output: {} tokens", profile.max_output_tokens);
    println!("  Source: {}", source.label());
    println!("  Chars/token (English): {:.1}", profile.chars_per_token_english);
    println!("  Chars/token (CJK): {:.1}", profile.chars_per_token_cjk);
    println!("  Message overhead: {} tokens", profile.message_overhead_tokens);
    println!("  System prompt overhead: {} tokens", profile.system_prompt_overhead_tokens);
    println!("  Tool definition overhead: {} tokens", profile.tool_definition_overhead_tokens);

    println!();
    0
}

/// List all entries in the built-in static model library (L3 fallback).
pub fn run_token_models() -> i32 {
    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Built-in Model Context Library     ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();
    println!("  3-layer resolution: user config > provider probe > this library");
    println!();

    println!("  {:<12} {:<22} {:>14} {:>12}",
        "Provider", "Model pattern", "Context(K)", "MaxOut");
    println!("  {}{}{}{}",
        "─".repeat(12), "─".repeat(22), "─".repeat(14), "─".repeat(12));

    for entry in BUILTIN_MODEL_CONTEXT.iter() {
        println!("  {:<12} {:<22} {:>14} {:>12}",
            entry.provider,
            entry.model_id,
            entry.context_window / 1000,
            entry.max_output_tokens,
        );
    }

    println!();
    println!("  Total entries: {}", BUILTIN_MODEL_CONTEXT.len());
    println!();
    0
}

/// Check if text fits within a model's context window.
pub fn run_token_fits(text: &str, model: &str) -> i32 {
    let counter = Arc::new(HeuristicTokenCounter::new()) as Arc<dyn TokenCounter>;
    let context_manager = ContextManager::new(counter.clone(), ContextTrimmingStrategy::default());

    // Create a sample conversation from the text
    let mut conv = oneai_core::Conversation::new();
    conv.add_message(oneai_core::Message::system("System prompt".to_string()));
    conv.add_message(oneai_core::Message::user(text.to_string()));

    let fit = context_manager.fits_context_window(&conv, model);

    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Context Fit Check                   ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    println!("  Model: {}", model);
    println!("  {}", fit.summary());
    println!();

    if fit.fits {
        println!("  ✓ Conversation fits within context window");
        println!("    Total tokens: {}", fit.total_tokens);
        println!("    Remaining: {}K tokens", fit.remaining_tokens / 1000);
    } else {
        println!("  ✗ Conversation exceeds context window!");
        println!("    Total tokens: {}", fit.total_tokens);
        println!("    Overflow: {}K tokens", fit.overflow_tokens / 1000);
    }

    println!();
    0
}

/// Estimate tokens in a sample conversation.
pub fn run_token_estimate(model: Option<&str>) -> i32 {
    let counter = HeuristicTokenCounter::new();
    let model_name = model.unwrap_or("gpt-4o");

    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Token Estimate                      ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    // Create a sample conversation
    let mut conv = oneai_core::Conversation::new();
    conv.add_message(oneai_core::Message::system("You are a helpful assistant.".to_string()));
    conv.add_message(oneai_core::Message::user("What is Rust programming language?".to_string()));
    conv.add_message(oneai_core::Message::assistant("Rust is a modern programming language focused on safety, speed, and concurrency.".to_string()));
    conv.add_message(oneai_core::Message::user("How does it compare to C++?".to_string()));

    let tokens = counter.count_conversation_tokens(&conv, model_name);
    let context_window = counter.context_window_size(model_name);
    let fit = counter.fits_context_window(&conv, model_name, 0.8);

    println!("  Sample conversation: {} messages", conv.len());
    println!("  Model: {}", model_name);
    println!("  Estimated tokens: {}", tokens);
    println!("  Context window: {}K tokens", context_window / 1000);
    println!("  {}", fit.summary());

    println!();
    0
}

/// Probe a provider's model-metadata endpoint for the context window (L2),
/// showing the full 3-layer resolution and which layer won.
///
/// Requires a configured provider (config file or `ONEAI_API_KEY`/`ONEAI_BASE_URL`/
/// `ONEAI_MODEL` env vars). Without one, only the L1/L3 layers are shown.
pub async fn run_token_probe(model: Option<&str>, config: &crate::config::OneaiConfig) -> i32 {
    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Context Window Probe (3-layer)      ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    // Build a ModelConfig from CLI config + --model override.
    let model_config = config.to_model_config_with_overrides(model);
    let model_name = model
        .map(|s| s.to_string())
        .or_else(|| model_config.as_ref().and_then(|c| c.model_name.clone()))
        .unwrap_or_else(|| config.provider.model.clone());

    println!("  Model: {}", model_name);

    // Layer candidates (sync, no provider needed).
    let resolver = ModelContextResolver::empty();
    let l1_env = std::env::var("ONEAI_CONTEXT_WINDOW").ok().and_then(|s| s.parse::<u32>().ok());
    let l3_builtin = builtin_lookup(&model_name).map(|e| e.context_window);

    println!();
    println!("  Layer candidates:");
    println!("    L1 env override   : {}", l1_env.map(|v| format!("{}K", v / 1000)).unwrap_or_else(|| "—".to_string()));
    println!("    L2 provider probe : {}", if model_config.is_some() { "pending (will query)" } else { "skipped (no provider configured)" });
    println!("    L3 builtin library: {}", l3_builtin.map(|v| format!("{}K", v / 1000)).unwrap_or_else(|| "—".to_string()));

    // If a provider is configured, perform the live L2 probe.
    let (resolved, source) = if let Some(mc) = model_config {
        let provider = oneai_provider::ProviderFactory::create(mc);
        let provider = Arc::from(provider);
        let r = ModelContextResolver::empty();
        r.resolve_with_source_with_provider(&model_name, &provider).await
    } else {
        // No provider — resolve via L1/L3 only.
        resolver.resolve_with_source(&model_name)
    };

    println!();
    println!("  ✓ Resolved context window: {}K tokens", resolved / 1000);
    println!("    Source: {}", source.label());
    println!();
    0
}
