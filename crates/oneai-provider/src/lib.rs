//! # OneAI Provider
//!
//! LLM provider implementations (OpenAI-compatible, Anthropic Claude, Google Gemini, Ollama)
//! and cost-based model routing.

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.

pub mod anthropic;
pub mod compat;
pub mod gemini;
pub mod model_router;
pub mod ollama;
pub mod openai;
pub mod provider_factory;
pub mod provider_pool;
pub mod retry;
pub mod smart_router;

pub use anthropic::AnthropicProvider;
pub use compat::{normalize_base_url, Compat, CompatFamily};
pub use gemini::GeminiProvider;
pub use model_router::{ModelRouter, RouteDecision, RouteProviderKind, RouteRule};
pub use ollama::OllamaProvider;
pub use oneai_core::ContextFitResult;
pub use oneai_core::ContextManager;
pub use oneai_core::ContextManagerConfig;
pub use oneai_core::ContextTrimmingStrategy;
pub use oneai_core::ContextWindowProfile;
pub use oneai_core::HeuristicTokenCounter;
pub use oneai_core::TokenCounter;
pub use openai::OpenAIProvider;
pub use provider_factory::ProviderFactory;
pub use provider_pool::{ProviderEntry, ProviderPool};
pub use retry::ProviderRetryConfig;
pub use smart_router::SmartRouter;

/// Merge adjacent [`oneai_core::Role::System`] messages into a single system
/// message (text joined with `\n\n`), so a provider's wire request carries one
/// `system` block per contiguous region instead of one per injected context
/// source / pinned block. Non-system messages break the run and pass through
/// untouched.
///
/// This is a **serialization-time** coalesce only: the in-memory `Conversation`
/// keeps the per-section messages (the issue #40 trajectory panel parses them
/// by `[Context: key]` / `[Task Anchor]` / `…` prefixes), so no consumer of the
/// assembled context loses section fidelity. Anthropic/Gemini already fold all
/// system messages into a single `system` / `systemInstruction` field, so only
/// the OpenAI-compatible and Ollama request builders call this.
pub fn merge_adjacent_system_messages(
    messages: &[oneai_core::Message],
) -> Vec<oneai_core::Message> {
    let mut merged: Vec<oneai_core::Message> = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.role == oneai_core::Role::System {
            if let Some(last) = merged.last_mut() {
                if last.role == oneai_core::Role::System {
                    let prev = last.text_content();
                    let next = msg.text_content();
                    let joined = if prev.is_empty() {
                        next
                    } else if next.is_empty() {
                        prev
                    } else {
                        format!("{prev}\n\n{next}")
                    };
                    last.content = vec![oneai_core::ContentBlock::Text { text: joined }];
                    continue;
                }
            }
        }
        merged.push(msg.clone());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::{Message, Role};

    #[test]
    fn merges_adjacent_system_messages_only() {
        let msgs = vec![
            Message::system("[Task Anchor] x"),
            Message::system("[Context: git_status] M a.rs"),
            Message::user("hi"),
            Message::assistant("hello"),
            Message::system("[Context: date] today"),
            Message::system("[Paradigm switch]: REACT"),
        ];
        let merged = merge_adjacent_system_messages(&msgs);

        // 6 → 4: two adjacent system runs collapse to one each.
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0].role, Role::System);
        assert!(merged[0].text_content().contains("[Task Anchor] x"));
        assert!(merged[0]
            .text_content()
            .contains("[Context: git_status] M a.rs"));
        assert_eq!(merged[1].role, Role::User);
        assert_eq!(merged[2].role, Role::Assistant);
        assert_eq!(merged[3].role, Role::System);
        assert!(merged[3].text_content().contains("[Context: date] today"));
        assert!(merged[3]
            .text_content()
            .contains("[Paradigm switch]: REACT"));
    }

    #[test]
    fn leaves_non_adjacent_and_non_system_untouched() {
        let msgs = vec![
            Message::system("a"),
            Message::user("u"),
            Message::system("b"),
            Message::tool_result("call".into(), "out".into()),
            Message::system("c"),
        ];
        let merged = merge_adjacent_system_messages(&msgs);
        assert_eq!(merged.len(), 5, "no adjacency → nothing merged");
    }

    #[test]
    fn empty_system_text_does_not_double_join() {
        let msgs = vec![
            Message::system("a"),
            Message::system(""),
            Message::system("b"),
        ];
        let merged = merge_adjacent_system_messages(&msgs);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text_content(), "a\n\nb");
    }
}
