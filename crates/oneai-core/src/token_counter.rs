//! Token counting — model-aware and provider-aware token estimation.
//!
//! The `TokenCounter` trait provides a unified interface for counting tokens
//! across different LLM providers. Each provider uses a different tokenizer
//! (OpenAI uses tiktoken/BPE, Anthropic uses its own tokenizer, Google uses
//! SentencePiece, Ollama varies by model). This module provides:
//!
//! - `TokenCounter` trait: abstract token counting interface
//! - `HeuristicTokenCounter`: improved per-provider heuristic estimation
//! - `ContextFitResult`: whether a conversation fits within a model's context window
//! - `ModelTokenizerProfile`: per-model tokenizer metadata
//! - `ProviderTokenizerType`: classification of how each provider tokenizes
//!
//! The `HeuristicTokenCounter` improves over the simple ~4 chars/token heuristic
//! by using per-provider family estimates and language detection:
//! - OpenAI: ~4.0 chars/token (English), ~2.0 chars/token (CJK)
//! - Anthropic: ~3.8 chars/token (English), ~1.8 chars/token (CJK)
//! - Google/Gemini: ~4.0 chars/token (English)
//! - Ollama: varies by model, default ~4.0 chars/token
//!
//! It also accounts for per-message overhead (role markers, formatting tokens)
//! that the simple heuristic ignores.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::Conversation;
use crate::ContentBlock;
use crate::Message;

// ─── TokenCounter trait ────────────────────────────────────────────────────

/// Trait for counting tokens — model-aware and provider-aware.
///
/// Implementations count tokens differently per provider:
/// - OpenAI uses tiktoken (BPE-based)
/// - Anthropic uses its own tokenizer
/// - Google uses SentencePiece
/// - Ollama varies by model
///
/// The `HeuristicTokenCounter` provides reasonable estimates without
/// requiring provider-specific tokenizer libraries.
pub trait TokenCounter: Send + Sync {
    /// Count tokens in a text string for a specific model.
    ///
    /// Uses the model name to determine the appropriate tokenizer family
    /// and estimation parameters. For unknown models, falls back to
    /// the default profile.
    fn count_tokens(&self, text: &str, model: &str) -> u32;

    /// Count tokens in a conversation for a specific model.
    ///
    /// Includes per-message overhead (role markers, formatting, separators)
    /// that provider APIs add when converting the conversation to their
    /// internal format. This overhead is typically ~4-8 tokens per message.
    fn count_conversation_tokens(&self, conversation: &Conversation, model: &str) -> u32;

    /// Get the context window size for a model.
    ///
    /// Returns the maximum number of tokens the model can process
    /// in a single inference call (input + output combined).
    fn context_window_size(&self, model: &str) -> u32;

    /// Check if a conversation fits within a model's context window.
    ///
    /// The `threshold` parameter (0.0 to 1.0) controls how much of the
    /// context window to use. A threshold of 0.8 means the conversation
    /// must use ≤80% of the context window, leaving 20% headroom for
    /// new tokens. This prevents sending requests that would overflow
    /// the model's context limit.
    fn fits_context_window(
        &self,
        conversation: &Conversation,
        model: &str,
        threshold: f64,
    ) -> ContextFitResult;
}

// ─── ContextFitResult ────────────────────────────────────────────────────

/// Result of a context fit check — whether a conversation fits within a model's context window.
///
/// Provides detailed information about the fit status, including token counts,
/// utilization percentage, and remaining/overflow tokens. This is used by
/// SmartRouter's context-aware routing and ContextManager's trimming logic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContextFitResult {
    /// Whether the conversation fits within the context window (at the given threshold).
    pub fits: bool,

    /// Total estimated tokens in the conversation.
    pub total_tokens: u32,

    /// The model's context window size.
    pub context_window: u32,

    /// Remaining tokens (context_window - total_tokens).
    /// May be 0 if the conversation exceeds the context window.
    pub remaining_tokens: u32,

    /// Tokens that overflow the context window threshold.
    /// `total_tokens - (context_window * threshold)` if > 0, else 0.
    pub overflow_tokens: u32,

    /// Utilization percentage — `total_tokens / context_window * 100`.
    pub utilization_pct: f64,
}

impl ContextFitResult {
    /// Create a new ContextFitResult.
    pub fn new(
        total_tokens: u32,
        context_window: u32,
        threshold: f64,
    ) -> Self {
        let effective_limit = (context_window as f64 * threshold) as u32;
        let fits = total_tokens <= effective_limit;
        let remaining_tokens = effective_limit.saturating_sub(total_tokens);
        let overflow_tokens = total_tokens.saturating_sub(effective_limit);
        let utilization_pct = if context_window > 0 {
            (total_tokens as f64 / context_window as f64) * 100.0
        } else {
            0.0
        };

        Self {
            fits,
            total_tokens,
            context_window,
            remaining_tokens,
            overflow_tokens,
            utilization_pct,
        }
    }

    /// Human-readable summary of the fit result.
    pub fn summary(&self) -> String {
        if self.fits {
            format!(
                "OK — {} tokens / {}K context ({:.1}% utilized, {}K remaining)",
                self.total_tokens,
                self.context_window / 1000,
                self.utilization_pct,
                self.remaining_tokens / 1000,
            )
        } else {
            format!(
                "OVERFLOW — {} tokens / {}K context ({:.1}% utilized, {}K overflow)",
                self.total_tokens,
                self.context_window / 1000,
                self.utilization_pct,
                self.overflow_tokens / 1000,
            )
        }
    }
}

// ─── ProviderTokenizerType ────────────────────────────────────────────

/// Classification of how each LLM provider tokenizes text.
///
/// Different providers use different tokenization algorithms,
/// which affects how many tokens a given text produces:
/// - OpenAI: tiktoken (BPE-based) — typically ~4 chars/token for English
/// - Anthropic: custom tokenizer — typically ~3.8 chars/token for English
/// - Google: SentencePiece — typically ~4 chars/token for English
/// - Ollama: varies by model (llama uses BPE, qwen uses custom)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ProviderTokenizerType {
    /// OpenAI tokenizer (tiktoken/BPE).
    OpenAI,
    /// Anthropic tokenizer (Claude-specific).
    Anthropic,
    /// Google tokenizer (SentencePiece).
    Google,
    /// Ollama tokenizer (varies by model).
    Ollama,
    /// Generic/fallback tokenizer (default heuristic).
    Generic,
}

impl ProviderTokenizerType {
    /// Infer tokenizer type from model name patterns.
    ///
    /// Uses naming conventions to classify:
    /// - "claude", "anthropic" → Anthropic
    /// - "gpt", "o3", "openai" → OpenAI
    /// - "gemini", "google" → Google
    /// - "ollama", "llama", "qwen" → Ollama
    /// - anything else → Generic
    pub fn from_model_name(model: &str) -> Self {
        let lower = model.to_lowercase();
        if lower.contains("claude") || lower.contains("anthropic") {
            Self::Anthropic
        } else if lower.contains("gpt") || lower.contains("o3") || lower.contains("openai") {
            Self::OpenAI
        } else if lower.contains("gemini") || lower.contains("google") {
            Self::Google
        } else if lower.contains("ollama") || lower.contains("llama") || lower.contains("qwen")
            || lower.contains("deepseek") || lower.contains("mistral")
            || lower.contains("glm") // GLM uses similar tokenization to Chinese-focused models
        {
            Self::Ollama
        } else {
            Self::Generic
        }
    }

    /// Default chars-per-token estimate for English text.
    ///
    /// | Type | English CPT | CJK CPT |
    /// |------|-------------|---------|
    /// | OpenAI | 4.0 | 2.0 |
    /// | Anthropic | 3.8 | 1.8 |
    /// | Google | 4.0 | 2.0 |
    /// | Ollama | 4.0 | 2.0 |
    /// | Generic | 4.0 | 2.0 |
    pub fn chars_per_token_english(&self) -> f64 {
        match self {
            Self::OpenAI => 4.0,
            Self::Anthropic => 3.8,
            Self::Google => 4.0,
            Self::Ollama => 4.0,
            Self::Generic => 4.0,
        }
    }

    /// Default chars-per-token estimate for CJK (Chinese/Japanese/Korean) text.
    pub fn chars_per_token_cjk(&self) -> f64 {
        match self {
            Self::OpenAI => 2.0,
            Self::Anthropic => 1.8,
            Self::Google => 2.0,
            Self::Ollama => 2.0,
            Self::Generic => 2.0,
        }
    }

    /// Human-readable name.
    pub fn name(&self) -> &str {
        match self {
            Self::OpenAI => "OpenAI (tiktoken/BPE)",
            Self::Anthropic => "Anthropic (Claude tokenizer)",
            Self::Google => "Google (SentencePiece)",
            Self::Ollama => "Ollama (varies by model)",
            Self::Generic => "Generic (heuristic)",
        }
    }
}

// ─── LanguageType ────────────────────────────────────────────────────────

/// Classification of text language for token estimation.
///
/// CJK (Chinese/Japanese/Korean) text has a different chars-per-token
/// ratio compared to Latin text. This classification enables
/// language-aware token estimation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageType {
    /// Predominantly Latin/English text.
    Latin,
    /// Predominantly CJK (Chinese/Japanese/Korean) text.
    CJK,
    /// Mixed Latin and CJK text.
    Mixed,
}

impl LanguageType {
    /// Detect the language type of a text string.
    ///
    /// Uses Unicode character ranges to classify:
    /// - CJK: characters in U+4E00-U+9FFF (Chinese), U+3040-U+309F (Hiragana),
    ///   U+30A0-U+30FF (Katakana), U+AC00-U+D7AF (Korean Hangul)
    /// - Latin: characters in basic ASCII + extended Latin ranges
    /// - Mixed: both CJK and Latin characters present
    pub fn detect(text: &str) -> Self {
        let mut cjk_count = 0;
        let mut latin_count = 0;

        for ch in text.chars() {
            // CJK Unified Ideographs
            if (ch >= '\u{4E00}' && ch <= '\u{9FFF}')
                // CJK Extension A
                || (ch >= '\u{3400}' && ch <= '\u{4DBF}')
                // Hiragana
                || (ch >= '\u{3040}' && ch <= '\u{309F}')
                // Katakana
                || (ch >= '\u{30A0}' && ch <= '\u{30FF}')
                // Korean Hangul Syllables
                || (ch >= '\u{AC00}' && ch <= '\u{D7AF}')
                // Korean Hangul Jamo
                || (ch >= '\u{1100}' && ch <= '\u{11FF}')
                // Fullwidth forms
                || (ch >= '\u{FF00}' && ch <= '\u{FFEF}')
            {
                cjk_count += 1;
            } else if ch.is_ascii_alphanumeric() || ch.is_ascii_punctuation() || ch == ' '
                // Extended Latin
                || (ch >= '\u{00C0}' && ch <= '\u{024F}')
            {
                latin_count += 1;
            }
        }

        if cjk_count == 0 && latin_count > 0 {
            Self::Latin
        } else if latin_count == 0 && cjk_count > 0 {
            Self::CJK
        } else if cjk_count > 0 && latin_count > 0 {
            // If >30% CJK, treat as CJK-dominant
            if cjk_count as f64 / (cjk_count + latin_count) as f64 > 0.3 {
                Self::CJK
            } else {
                Self::Mixed
            }
        } else {
            Self::Latin // Default for empty text
        }
    }
}

// ─── ModelTokenizerProfile ────────────────────────────────────────────────

/// Per-model tokenizer metadata for token estimation.
///
/// Each model has different tokenization characteristics:
/// - Different chars-per-token ratios
/// - Different per-message overhead (role markers, formatting)
/// - Different context window sizes
/// - Different max output token limits
///
/// The `HeuristicTokenCounter` uses these profiles to produce
/// model-aware token estimates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelTokenizerProfile {
    /// Model name (e.g., "claude-opus-4-8", "gpt-4o").
    pub model_name: String,

    /// Provider tokenizer type.
    pub tokenizer_type: ProviderTokenizerType,

    /// Estimated chars per token for English/Latin text.
    pub chars_per_token_english: f64,

    /// Estimated chars per token for CJK text.
    pub chars_per_token_cjk: f64,

    /// Per-message overhead in tokens (role markers, separators, formatting).
    /// OpenAI: ~4 tokens/message, Anthropic: ~6 tokens/message.
    pub message_overhead_tokens: u32,

    /// System prompt overhead in tokens.
    pub system_prompt_overhead_tokens: u32,

    /// Tool call definition overhead in tokens (per tool).
    pub tool_definition_overhead_tokens: u32,

    /// Maximum context window size in tokens.
    pub context_window_tokens: u32,

    /// Maximum output tokens.
    pub max_output_tokens: u32,
}

impl ModelTokenizerProfile {
    /// Create a new profile.
    pub fn new(
        model_name: impl Into<String>,
        tokenizer_type: ProviderTokenizerType,
        chars_per_token_english: f64,
        chars_per_token_cjk: f64,
        message_overhead_tokens: u32,
        system_prompt_overhead_tokens: u32,
        tool_definition_overhead_tokens: u32,
        context_window_tokens: u32,
        max_output_tokens: u32,
    ) -> Self {
        Self {
            model_name: model_name.into(),
            tokenizer_type,
            chars_per_token_english,
            chars_per_token_cjk,
            message_overhead_tokens,
            system_prompt_overhead_tokens,
            tool_definition_overhead_tokens,
            context_window_tokens,
            max_output_tokens,
        }
    }

    /// Create a profile with defaults inferred from model name.
    ///
    /// Uses `ProviderTokenizerType::from_model_name()` to determine
    /// the tokenizer family, then applies default values for that family.
    /// Context window and max output are inferred from model name patterns.
    pub fn from_model_name(model: &str) -> Self {
        let tokenizer_type = ProviderTokenizerType::from_model_name(model);
        let (cpt_en, cpt_cjk, msg_overhead, sys_overhead, tool_overhead) =
            tokenizer_type_default_overhead(&tokenizer_type);
        let context_window = infer_context_window_for_tokenizer(model);
        let max_output = infer_max_output_for_tokenizer(model);

        Self {
            model_name: model.to_string(),
            tokenizer_type,
            chars_per_token_english: cpt_en,
            chars_per_token_cjk: cpt_cjk,
            message_overhead_tokens: msg_overhead,
            system_prompt_overhead_tokens: sys_overhead,
            tool_definition_overhead_tokens: tool_overhead,
            context_window_tokens: context_window,
            max_output_tokens: max_output,
        }
    }

    /// Build default profiles for all known models.
    ///
    /// Matches the 12 models from `ModelQualityProfile::default_profiles()`.
    pub fn default_profiles() -> Vec<Self> {
        vec![
            // Anthropic family
            Self::from_model_name("claude-haiku-4-5-20251001"),
            Self::from_model_name("claude-sonnet-4-6-20250514"),
            Self::from_model_name("claude-opus-4-8"),
            // OpenAI family
            Self::from_model_name("gpt-4o-mini"),
            Self::from_model_name("gpt-4o"),
            Self::from_model_name("o3-pro"),
            // Google family
            Self::from_model_name("gemini-2.0-flash"),
            Self::from_model_name("gemini-2.5-flash"),
            Self::from_model_name("gemini-2.5-pro"),
            // Ollama family
            Self::from_model_name("qwen2.5:0.5b"),
            Self::from_model_name("qwen2.5:7b"),
            Self::from_model_name("deepseek-r1:14b"),
        ]
    }

    /// Get the effective chars-per-token for text based on its language.
    ///
    /// For mixed-language text, uses a weighted blend of English and CJK ratios.
    pub fn chars_per_token_for_text(&self, text: &str) -> f64 {
        let lang = LanguageType::detect(text);
        match lang {
            LanguageType::Latin => self.chars_per_token_english,
            LanguageType::CJK => self.chars_per_token_cjk,
            LanguageType::Mixed => {
                // Blend: weighted by estimated CJK proportion
                // For mixed text, use a compromise ratio
                self.chars_per_token_english * 0.5 + self.chars_per_token_cjk * 0.5
            }
        }
    }
}

// ─── HeuristicTokenCounter ────────────────────────────────────────────

/// Heuristic token counter — improved per-provider estimation.
///
/// Improves over the flat ~4 chars/token heuristic by:
/// 1. Using per-provider family estimates (OpenAI, Anthropic, Google, Ollama)
/// 2. Detecting CJK vs Latin text for better ratio estimation
/// 3. Adding per-message overhead (role markers, formatting tokens)
/// 4. Supporting custom profiles for non-default models
///
/// This is suitable for production use when exact token counting
/// (via tiktoken or provider APIs) is not available. The estimates
/// are typically within ±10% of actual token counts for English text.
pub struct HeuristicTokenCounter {
    /// Per-model tokenizer profiles.
    profiles: HashMap<String, ModelTokenizerProfile>,

    /// Default profile for unknown models.
    default_profile: ModelTokenizerProfile,
}

impl HeuristicTokenCounter {
    /// Create a new heuristic token counter with default profiles.
    ///
    /// Includes profiles for the 12 known models from
    /// `ModelTokenizerProfile::default_profiles()`.
    pub fn new() -> Self {
        let profiles = Self::default_profiles_map();
        let default_profile = ModelTokenizerProfile::from_model_name("default");

        Self { profiles, default_profile }
    }

    /// Create with only default profile (no model-specific profiles).
    pub fn with_default_only() -> Self {
        let default_profile = ModelTokenizerProfile::from_model_name("default");
        Self {
            profiles: HashMap::new(),
            default_profile,
        }
    }

    /// Create with custom profiles (replaces default profiles).
    pub fn with_profiles(profiles: Vec<ModelTokenizerProfile>) -> Self {
        let map = profiles.into_iter()
            .map(|p| (p.model_name.clone(), p))
            .collect();
        let default_profile = ModelTokenizerProfile::from_model_name("default");
        Self { profiles: map, default_profile }
    }

    /// Add a custom model profile.
    pub fn add_profile(&mut self, profile: ModelTokenizerProfile) {
        self.profiles.insert(profile.model_name.clone(), profile);
    }

    /// Get the profile for a model, or the default profile if unknown.
    pub fn profile_for_model(&self, model: &str) -> &ModelTokenizerProfile {
        self.profiles.get(model).unwrap_or(&self.default_profile)
    }

    /// Build the default profiles map.
    fn default_profiles_map() -> HashMap<String, ModelTokenizerProfile> {
        ModelTokenizerProfile::default_profiles().into_iter()
            .map(|p| (p.model_name.clone(), p))
            .collect()
    }
}

impl Default for HeuristicTokenCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenCounter for HeuristicTokenCounter {
    fn count_tokens(&self, text: &str, model: &str) -> u32 {
        let profile = self.profile_for_model(model);
        let chars_per_token = profile.chars_per_token_for_text(text);

        if chars_per_token <= 0.0 {
            // Safety fallback
            return (text.chars().count() as f64 / 4.0).ceil() as u32;
        }

        (text.chars().count() as f64 / chars_per_token).ceil() as u32
    }

    fn count_conversation_tokens(&self, conversation: &Conversation, model: &str) -> u32 {
        let profile = self.profile_for_model(model);
        let mut total_tokens = 0u32;

        // System prompt overhead (added once at the start)
        total_tokens += profile.system_prompt_overhead_tokens;

        for msg in &conversation.messages {
            // Per-message overhead (role markers, separators, formatting)
            total_tokens += profile.message_overhead_tokens;

            // Count content tokens
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        let cpt = profile.chars_per_token_for_text(text);
                        if cpt > 0.0 {
                            total_tokens += (text.chars().count() as f64 / cpt).ceil() as u32;
                        } else {
                            total_tokens += (text.chars().count() as f64 / 4.0).ceil() as u32;
                        }
                    }
                    ContentBlock::ToolCall { name, args, .. } => {
                        // Tool call: name + args + formatting overhead
                        let name_tokens = self.count_tokens(name, model);
                        let args_tokens = self.count_tokens(args, model);
                        total_tokens += name_tokens + args_tokens + profile.tool_definition_overhead_tokens;
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        let cpt = profile.chars_per_token_for_text(content);
                        if cpt > 0.0 {
                            total_tokens += (content.chars().count() as f64 / cpt).ceil() as u32;
                        } else {
                            total_tokens += (content.chars().count() as f64 / 4.0).ceil() as u32;
                        }
                        // Tool result formatting overhead
                        total_tokens += 4; // call_id reference + formatting
                    }
                    ContentBlock::Image { .. } => {
                        // Image tokens: roughly 85-170 tokens per image (low/high detail)
                        // Use 170 as conservative estimate
                        total_tokens += 170;
                    }
                    ContentBlock::Thinking { text, .. } => {
                        total_tokens += self.count_tokens(text, model);
                    }
                    ContentBlock::File { .. } => {
                        // File reference: roughly 50 tokens (mime type + URI + formatting)
                        total_tokens += 50;
                    }
                }
            }
        }

        total_tokens
    }

    fn context_window_size(&self, model: &str) -> u32 {
        let profile = self.profile_for_model(model);
        // For known models in the profiles map, use the stored value.
        // For unknown models (which get the default profile), dynamically infer
        // from model name patterns — this handles models like "glm-5.1" that
        // aren't in the default profiles but have known context windows.
        if self.profiles.contains_key(model) {
            profile.context_window_tokens
        } else {
            infer_context_window_for_tokenizer(model)
        }
    }

    fn fits_context_window(
        &self,
        conversation: &Conversation,
        model: &str,
        threshold: f64,
    ) -> ContextFitResult {
        let total_tokens = self.count_conversation_tokens(conversation, model);
        let context_window = self.context_window_size(model);
        ContextFitResult::new(total_tokens, context_window, threshold)
    }
}

// ─── Helper functions ──────────────────────────────────────────────────

/// Get default overhead values for a tokenizer type.
fn tokenizer_type_default_overhead(tokenizer_type: &ProviderTokenizerType) -> (f64, f64, u32, u32, u32) {
    let cpt_en = tokenizer_type.chars_per_token_english();
    let cpt_cjk = tokenizer_type.chars_per_token_cjk();
    let (msg_overhead, sys_overhead, tool_overhead) = match tokenizer_type {
        ProviderTokenizerType::OpenAI => (4, 8, 6),
        ProviderTokenizerType::Anthropic => (6, 10, 8),
        ProviderTokenizerType::Google => (4, 8, 6),
        ProviderTokenizerType::Ollama => (4, 8, 6),
        ProviderTokenizerType::Generic => (4, 8, 6),
    };
    (cpt_en, cpt_cjk, msg_overhead, sys_overhead, tool_overhead)
}

/// Infer context window size from model name.
///
/// Uses naming conventions to determine context window:
/// - "opus", "sonnet" → 200K
/// - "haiku" → 128K
/// - "gpt-4o", "o3" → 128K-200K
/// - "gemini" → 1M (Gemini has very large context windows)
/// - "qwen", "llama" → 32K
/// - "deepseek" → 64K
pub fn infer_context_window_for_tokenizer(model: &str) -> u32 {
    // Check explicit override first (highest priority)
    if let Ok(size) = std::env::var("ONEAI_CONTEXT_WINDOW") {
        if let Ok(val) = size.parse::<u32>() {
            return val;
        }
    }

    let lower = model.to_lowercase();
    if lower.contains("gemini") {
        return 1_000_000;
    }
    if lower.contains("opus") || lower.contains("sonnet") || lower.contains("gpt-4") {
        return 200_000;
    }
    if lower.contains("haiku") || lower.contains("mini") || lower.contains("nano") {
        return 128_000;
    }
    if lower.contains("glm-4") || lower.contains("glm4") {
        return 128_000;
    }
    if lower.contains("glm-5") || lower.contains("glm5") || lower.contains("glm") {
        return 203_000;  // GLM-5.x series: ~203K context window
    }
    if lower.contains("deepseek-r1") {
        return 64_000;
    }
    if lower.contains("qwen") || lower.contains("llama") || lower.contains("ollama") {
        return 32_000;
    }
    if lower.contains("o3") {
        return 200_000;
    }
    128_000 // Default
}

/// Infer max output tokens from model name.
fn infer_max_output_for_tokenizer(model: &str) -> u32 {
    let lower = model.to_lowercase();
    if lower.contains("opus") || lower.contains("o3-pro") {
        16_384
    } else if lower.contains("sonnet") || lower.contains("gpt-4o") || lower.contains("gemini") {
        8_192
    } else if lower.contains("haiku") || lower.contains("mini") || lower.contains("flash") {
        4_096
    } else if lower.contains("deepseek-r1") {
        8_192
    } else if lower.contains("qwen") || lower.contains("llama") {
        4_096
    } else {
        4_096 // Default
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── HeuristicTokenCounter tests ──────────────────────────────────

    #[test]
    fn test_heuristic_token_counter_default_profiles() {
        let counter = HeuristicTokenCounter::new();
        // Should have 12 profiles
        assert!(counter.profiles.len() >= 12);

        // Check known models exist
        assert!(counter.profiles.contains_key("claude-opus-4-8"));
        assert!(counter.profiles.contains_key("gpt-4o"));
        assert!(counter.profiles.contains_key("gemini-2.5-pro"));
        assert!(counter.profiles.contains_key("qwen2.5:7b"));
    }

    #[test]
    fn test_heuristic_token_counter_count_tokens_english() {
        let counter = HeuristicTokenCounter::new();

        // "Hello world" — 11 chars, ~4 chars/token for OpenAI → ~3 tokens
        let tokens = counter.count_tokens("Hello world", "gpt-4o");
        assert!(tokens >= 2 && tokens <= 4);

        // Longer English text
        let tokens_anthropic = counter.count_tokens(
            "The quick brown fox jumps over the lazy dog",
            "claude-sonnet-4-6-20250514",
        );
        // 44 chars / 3.8 cpt ≈ 12 tokens
        assert!(tokens_anthropic >= 10 && tokens_anthropic <= 15);
    }

    #[test]
    fn test_heuristic_token_counter_count_tokens_cjk() {
        let counter = HeuristicTokenCounter::new();

        // Chinese text: "你好世界" — 4 CJK chars, ~1.8 chars/token for Anthropic → ~3 tokens
        let tokens = counter.count_tokens("你好世界", "claude-sonnet-4-6-20250514");
        assert!(tokens >= 2 && tokens <= 4);

        // Longer Chinese text
        let tokens_openai = counter.count_tokens(
            "这是一个很长的中文句子用来测试分词器的估算能力",
            "gpt-4o",
        );
        // 22 CJK chars / 2.0 cpt ≈ 11 tokens
        assert!(tokens_openai >= 9 && tokens_openai <= 14);
    }

    #[test]
    fn test_heuristic_token_counter_count_conversation_tokens() {
        let counter = HeuristicTokenCounter::new();
        let mut conv = Conversation::new();
        conv.add_message(Message::system("You are helpful".to_string()));
        conv.add_message(Message::user("What is Rust?".to_string()));

        let tokens = counter.count_conversation_tokens(&conv, "gpt-4o");
        // System prompt overhead: 8
        // Per-message overhead: 2 * 4 = 8
        // "You are helpful" ≈ 4 tokens
        // "What is Rust?" ≈ 4 tokens
        // Total ≈ 8 + 8 + 4 + 4 = ~24
        assert!(tokens >= 16 && tokens <= 30);
    }

    #[test]
    fn test_heuristic_token_counter_context_window_size() {
        // Most basic test first
        let lower = "gemini-2.5-pro".to_lowercase();
        assert!(lower.contains("gemini"), "lowercase must contain 'gemini': {}", lower);

        // Call infer_context_window directly
        let result = infer_context_window_for_tokenizer("gemini-2.5-pro");
        assert_eq!(result, 1_000_000, "infer_context_window_for_tokenizer returned {} for gemini-2.5-pro", result);

        let counter = HeuristicTokenCounter::new();
        assert_eq!(counter.context_window_size("claude-opus-4-8"), 200_000);
        assert_eq!(counter.context_window_size("claude-haiku-4-5-20251001"), 128_000);
        assert_eq!(counter.context_window_size("gpt-4o"), 200_000);
        assert_eq!(counter.context_window_size("gemini-2.5-pro"), 1_000_000);
        assert_eq!(counter.context_window_size("qwen2.5:7b"), 32_000);
        assert_eq!(counter.context_window_size("glm-5.1"), 203_000);
        assert_eq!(counter.context_window_size("glm-4-plus"), 128_000);
        assert_eq!(counter.context_window_size("unknown-model"), 128_000);
    }

    #[test]
    fn test_infer_context_window_glm_models() {
        assert_eq!(infer_context_window_for_tokenizer("glm-5.1"), 203_000);
        assert_eq!(infer_context_window_for_tokenizer("glm-5.1-plus"), 203_000);
        assert_eq!(infer_context_window_for_tokenizer("glm-4"), 128_000);
        assert_eq!(infer_context_window_for_tokenizer("glm-4-plus"), 128_000);
    }

    #[test]
    fn test_infer_context_window_env_override() {
        // ONEAI_CONTEXT_WINDOW env var overrides all model inference
        // Use a unique value to avoid collision with parallel tests
        std::env::set_var("ONEAI_CONTEXT_WINDOW", "999999");
        // Any model should return the env override value
        let result = infer_context_window_for_tokenizer("some-unknown-model-xyz");
        std::env::remove_var("ONEAI_CONTEXT_WINDOW");
        assert_eq!(result, 999_999);
    }

    #[test]
    fn test_heuristic_token_counter_detect_language() {
        assert_eq!(LanguageType::detect("Hello world"), LanguageType::Latin);
        assert_eq!(LanguageType::detect("你好世界"), LanguageType::CJK);
        // Mixed: "Hello 你好" has ~28% CJK, below 30% threshold → Mixed
        assert_eq!(LanguageType::detect("Hello 你好"), LanguageType::Mixed);
        // Predominantly CJK: enough CJK chars to pass 30% threshold
        assert_eq!(LanguageType::detect("Hi 你好世界测试"), LanguageType::CJK);
        assert_eq!(LanguageType::detect(""), LanguageType::Latin);
    }

    #[test]
    fn test_heuristic_token_counter_chars_per_token_per_provider() {
        let counter = HeuristicTokenCounter::new();

        // OpenAI model: ~4.0 cpt for English
        let profile = counter.profile_for_model("gpt-4o");
        assert!((profile.chars_per_token_english - 4.0).abs() < 0.01);

        // Anthropic model: ~3.8 cpt for English
        let profile = counter.profile_for_model("claude-sonnet-4-6-20250514");
        assert!((profile.chars_per_token_english - 3.8).abs() < 0.01);

        // Google model: ~4.0 cpt for English
        let profile = counter.profile_for_model("gemini-2.5-pro");
        assert!((profile.chars_per_token_english - 4.0).abs() < 0.01);
    }

    #[test]
    fn test_heuristic_token_counter_message_overhead() {
        let counter = HeuristicTokenCounter::new();

        // OpenAI: 4 tokens per message
        let openai_profile = counter.profile_for_model("gpt-4o");
        assert_eq!(openai_profile.message_overhead_tokens, 4);
        assert_eq!(openai_profile.system_prompt_overhead_tokens, 8);

        // Anthropic: 6 tokens per message
        let anthropic_profile = counter.profile_for_model("claude-opus-4-8");
        assert_eq!(anthropic_profile.message_overhead_tokens, 6);
        assert_eq!(anthropic_profile.system_prompt_overhead_tokens, 10);
    }

    // ─── ContextFitResult tests ──────────────────────────────────

    #[test]
    fn test_context_fit_result_fits() {
        let result = ContextFitResult::new(50_000, 200_000, 0.8);
        assert!(result.fits);
        assert_eq!(result.total_tokens, 50_000);
        assert_eq!(result.context_window, 200_000);
        assert_eq!(result.remaining_tokens, 110_000); // 160K - 50K
        assert_eq!(result.overflow_tokens, 0);
        assert!((result.utilization_pct - 25.0).abs() < 1.0);
    }

    #[test]
    fn test_context_fit_result_overflow() {
        let result = ContextFitResult::new(170_000, 200_000, 0.8);
        assert!(!result.fits); // 170K > 160K (80% of 200K)
        assert_eq!(result.total_tokens, 170_000);
        assert_eq!(result.overflow_tokens, 10_000); // 170K - 160K
        assert!((result.utilization_pct - 85.0).abs() < 1.0);
    }

    #[test]
    fn test_context_fit_result_utilization_pct() {
        let result = ContextFitResult::new(100_000, 200_000, 0.8);
        assert!((result.utilization_pct - 50.0).abs() < 1.0);
        assert!(result.fits); // 100K < 160K (80% of 200K)
    }

    #[test]
    fn test_context_fit_result_summary() {
        let fits_result = ContextFitResult::new(50_000, 200_000, 0.8);
        assert!(fits_result.summary().contains("OK"));

        let overflow_result = ContextFitResult::new(170_000, 200_000, 0.8);
        assert!(overflow_result.summary().contains("OVERFLOW"));
    }

    // ─── ProviderTokenizerType tests ──────────────────────────────────

    #[test]
    fn test_provider_tokenizer_type_classification() {
        assert_eq!(
            ProviderTokenizerType::from_model_name("claude-opus-4-8"),
            ProviderTokenizerType::Anthropic
        );
        assert_eq!(
            ProviderTokenizerType::from_model_name("gpt-4o"),
            ProviderTokenizerType::OpenAI
        );
        assert_eq!(
            ProviderTokenizerType::from_model_name("gemini-2.5-pro"),
            ProviderTokenizerType::Google
        );
        assert_eq!(
            ProviderTokenizerType::from_model_name("qwen2.5:7b"),
            ProviderTokenizerType::Ollama
        );
        assert_eq!(
            ProviderTokenizerType::from_model_name("some-random-model"),
            ProviderTokenizerType::Generic
        );
    }

    // ─── ModelTokenizerProfile tests ──────────────────────────────────

    #[test]
    fn test_model_tokenizer_profile_creation() {
        let profile = ModelTokenizerProfile::new(
            "custom-model",
            ProviderTokenizerType::OpenAI,
            4.5,
            2.5,
            5,
            10,
            7,
            128_000,
            4_096,
        );
        assert_eq!(profile.model_name, "custom-model");
        assert_eq!(profile.tokenizer_type, ProviderTokenizerType::OpenAI);
        assert!((profile.chars_per_token_english - 4.5).abs() < 0.01);
        assert_eq!(profile.message_overhead_tokens, 5);
        assert_eq!(profile.context_window_tokens, 128_000);
    }

    #[test]
    fn test_heuristic_token_counter_custom_profile() {
        let mut counter = HeuristicTokenCounter::new();
        let custom = ModelTokenizerProfile::new(
            "my-custom-model",
            ProviderTokenizerType::Anthropic,
            3.0,
            1.5,
            5,
            8,
            4,
            50_000,
            2_048,
        );
        counter.add_profile(custom);

        assert_eq!(counter.context_window_size("my-custom-model"), 50_000);
        let tokens = counter.count_tokens("Hello", "my-custom-model");
        // 5 chars / 3.0 cpt ≈ 2 tokens
        assert!(tokens >= 1 && tokens <= 3);
    }

    #[test]
    fn test_heuristic_token_counter_fallback_to_default() {
        let counter = HeuristicTokenCounter::new();

        // Unknown model falls back to default (Generic type)
        let tokens = counter.count_tokens("Hello world", "unknown-model-xyz");
        assert!(tokens > 0);

        // Context window for unknown model → default 128K
        assert_eq!(counter.context_window_size("unknown-model-xyz"), 128_000);
    }

    #[test]
    fn test_model_tokenizer_profile_chars_per_token_for_text() {
        let profile = ModelTokenizerProfile::from_model_name("gpt-4o");

        // English text → chars_per_token_english
        let cpt = profile.chars_per_token_for_text("Hello world");
        assert!((cpt - 4.0).abs() < 0.01);

        // CJK text → chars_per_token_cjk
        let cpt = profile.chars_per_token_for_text("你好世界");
        assert!((cpt - 2.0).abs() < 0.01);
    }

    #[test]
    fn test_model_tokenizer_profile_default_profiles_count() {
        let profiles = ModelTokenizerProfile::default_profiles();
        assert_eq!(profiles.len(), 12); // 4 families × 3 tiers

        // Check each family has 3 models
        let anthropic_count = profiles.iter().filter(|p| p.tokenizer_type == ProviderTokenizerType::Anthropic).count();
        assert_eq!(anthropic_count, 3);

        let openai_count = profiles.iter().filter(|p| p.tokenizer_type == ProviderTokenizerType::OpenAI).count();
        assert_eq!(openai_count, 3);

        let google_count = profiles.iter().filter(|p| p.tokenizer_type == ProviderTokenizerType::Google).count();
        assert_eq!(google_count, 3);

        let ollama_count = profiles.iter().filter(|p| p.tokenizer_type == ProviderTokenizerType::Ollama).count();
        assert_eq!(ollama_count, 3);
    }
}
