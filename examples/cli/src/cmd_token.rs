//! CLI token subcommand — token counting, context window profiles, and fit checking.

use oneai_core::{HeuristicTokenCounter, TokenCounter, ModelTokenizerProfile, ProviderTokenizerType};
use oneai_core::{ContextManager, ContextTrimmingStrategy};
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

    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Context Window Profile              ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    println!("  Model: {}", profile.model_name);
    println!("  Tokenizer: {}", tokenizer_type.name());
    println!("  Context window: {}K tokens", profile.context_window_tokens / 1000);
    println!("  Max output: {} tokens", profile.max_output_tokens);
    println!("  Chars/token (English): {:.1}", profile.chars_per_token_english);
    println!("  Chars/token (CJK): {:.1}", profile.chars_per_token_cjk);
    println!("  Message overhead: {} tokens", profile.message_overhead_tokens);
    println!("  System prompt overhead: {} tokens", profile.system_prompt_overhead_tokens);
    println!("  Tool definition overhead: {} tokens", profile.tool_definition_overhead_tokens);

    println!();
    0
}

/// List all known tokenizer profiles.
pub fn run_token_models() -> i32 {
    let profiles = ModelTokenizerProfile::default_profiles();

    println!("╔══════════════════════════════════════════════════╗");
    println!("║        OneAI Tokenizer Profiles                  ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    println!("  {:<25} {:<15} {:>10} {:>10} {:>8} {:>8}",
        "Model", "Tokenizer", "Context(K)", "MaxOut", "CPT(EN)", "CPT(CJK)");
    println!("  {}{}{}{}{}{}",
        "─".repeat(25), "─".repeat(15), "─".repeat(10), "─".repeat(10), "─".repeat(8), "─".repeat(8));

    for profile in &profiles {
        println!("  {:<25} {:<15} {:>10} {:>10} {:>8.1} {:>8.1}",
            profile.model_name,
            profile.tokenizer_type.name().split('(').next().unwrap_or("").trim(),
            profile.context_window_tokens / 1000,
            profile.max_output_tokens,
            profile.chars_per_token_english,
            profile.chars_per_token_cjk,
        );
    }

    println!();
    println!("  Total profiles: {}", profiles.len());
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
