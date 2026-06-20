//! Context accounting — breakdown of token usage by category.
//!
//! Provides a structured breakdown of how the context window is occupied,
//! categorizing tokens into: system prompt, user messages, assistant messages,
//! tool calls, tool results, thinking, and free space.
//!
//! Uses `HeuristicTokenCounter` for model-aware, CJK-aware token estimation.
//! The sidebar and `/context` command both source their numbers from this
//! module, ensuring consistency.

use crate::Conversation;
use crate::ContentBlock;
use crate::Role;
use crate::token_counter::HeuristicTokenCounter;
use crate::token_counter::TokenCounter;

// ─── ContextAccounting ────────────────────────────────────────────────────

/// Breakdown of token usage by category within a conversation's context window.
///
/// Each field represents the estimated token count for that category.
/// `total_tokens` is the sum of all content categories + overhead.
/// `free_space` is `context_window_size - total_tokens`.
///
/// This is used by:
/// - The TUI sidebar to display `📝~ctx N%` (derived from `utilization_pct`)
/// - The `/context` command to show a detailed breakdown
/// - Both use the same `HeuristicTokenCounter` for consistency
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ContextAccounting {
    /// System prompt messages (Role::System).
    pub system_prompt_tokens: u32,

    /// User messages (Role::User).
    pub user_messages_tokens: u32,

    /// Assistant text content (Role::Assistant, ContentBlock::Text).
    pub assistant_messages_tokens: u32,

    /// Tool call definitions (ContentBlock::ToolCall: name + args).
    pub tool_call_tokens: u32,

    /// Tool result outputs (Role::Tool / ContentBlock::ToolResult).
    pub tool_result_tokens: u32,

    /// Thinking/reasoning content (ContentBlock::Thinking).
    pub thinking_tokens: u32,

    /// Image content (ContentBlock::Image).
    pub image_tokens: u32,

    /// File content (ContentBlock::File).
    pub file_tokens: u32,

    /// Total estimated tokens (all categories + per-message overhead).
    pub total_tokens: u32,

    /// Model's context window size.
    pub context_window_size: u32,

    /// Remaining tokens (context_window_size - total_tokens).
    pub free_space: u32,

    /// Utilization percentage — total_tokens / context_window_size * 100.
    pub utilization_pct: f64,

    /// Whether these are heuristic estimates (not from real API usage data).
    pub is_estimated: bool,
}

impl ContextAccounting {
    /// Compute a context accounting from a conversation.
    ///
    /// Uses `HeuristicTokenCounter` to estimate tokens per category, accounting
    /// for provider-specific chars-per-token ratios, CJK text, and per-message
    /// overhead. `tool_defs_count` is the number of tool definitions that would
    /// be included in the inference request (each adds overhead tokens).
    ///
    /// The `model_name` determines which tokenizer profile to use. If the model
    /// is unknown, the default (Generic) profile is used.
    pub fn account(
        conversation: &Conversation,
        model_name: &str,
        tool_defs_count: usize,
    ) -> Self {
        let counter = HeuristicTokenCounter::new();
        let profile = counter.profile_for_model(model_name);

        let mut accounting = Self::default();
        accounting.context_window_size = counter.context_window_size(model_name);
        accounting.is_estimated = true;

        let mut system_overhead_added = false;

        for msg in &conversation.messages {
            // Per-message overhead (role markers, separators, formatting)
            let mut msg_overhead = profile.message_overhead_tokens;

            // System prompt overhead (added once for the first system message)
            if msg.role == Role::System && !system_overhead_added {
                msg_overhead += profile.system_prompt_overhead_tokens;
                system_overhead_added = true;
            }

            // Categorize each content block
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        let tokens = counter.count_tokens(text, model_name);
                        match msg.role {
                            Role::System => accounting.system_prompt_tokens += tokens,
                            Role::User => accounting.user_messages_tokens += tokens,
                            Role::Assistant => accounting.assistant_messages_tokens += tokens,
                            Role::Tool => accounting.tool_result_tokens += tokens,
                        }
                    }
                    ContentBlock::ToolCall { name, args, .. } => {
                        let name_tokens = counter.count_tokens(name, model_name);
                        let args_tokens = counter.count_tokens(args, model_name);
                        let overhead = profile.tool_definition_overhead_tokens;
                        accounting.tool_call_tokens += name_tokens + args_tokens + overhead;
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        let tokens = counter.count_tokens(content, model_name);
                        accounting.tool_result_tokens += tokens + 4; // formatting overhead
                    }
                    ContentBlock::Thinking { text } => {
                        accounting.thinking_tokens += counter.count_tokens(text, model_name);
                    }
                    ContentBlock::Image { .. } => {
                        accounting.image_tokens += 170; // conservative estimate
                    }
                    ContentBlock::File { .. } => {
                        accounting.file_tokens += 50; // mime + URI + formatting
                    }
                }
            }

            // Add per-message overhead to total
            accounting.total_tokens += msg_overhead;
        }

        // Tool definitions overhead (each tool def adds overhead tokens)
        accounting.tool_call_tokens += (tool_defs_count as u32) * profile.tool_definition_overhead_tokens;

        // Sum all content categories into total
        accounting.total_tokens += accounting.system_prompt_tokens
            + accounting.user_messages_tokens
            + accounting.assistant_messages_tokens
            + accounting.tool_call_tokens
            + accounting.tool_result_tokens
            + accounting.thinking_tokens
            + accounting.image_tokens
            + accounting.file_tokens;

        // Compute free space and utilization
        accounting.free_space = accounting.context_window_size.saturating_sub(accounting.total_tokens);
        accounting.utilization_pct = if accounting.context_window_size > 0 {
            (accounting.total_tokens as f64 / accounting.context_window_size as f64) * 100.0
        } else {
            0.0
        };

        accounting
    }

    /// Format a human-readable breakdown for the `/context` command.
    ///
    /// Produces a Claude Code-style display with:
    /// - Model name and total/window
    /// - Category breakdown with tokens and percentage
    /// - Free space indication
    /// - Visual bar (Unicode block characters)
    pub fn format_display(&self, provider_info: &str) -> String {
        let ctx_window_k = self.context_window_size as f64 / 1000.0;
        let total_k = format_token_k(self.total_tokens);
        let free_k = format_token_k(self.free_space);

        let mut lines = Vec::new();

        // Header
        lines.push("📊 Context Usage".to_string());
        lines.push(String::new());
        lines.push(format!("Model: {}", provider_info));

        // Visual bar
        let bar_width = 20;
        let ratio = if self.context_window_size > 0 {
            self.total_tokens as f64 / self.context_window_size as f64
        } else {
            0.0
        };
        let filled = (ratio * bar_width as f64).round() as usize;
        let empty = bar_width - filled;
        let bar = format!(
            "  [{}{}] {}{} / {:.0}k ({:.1}%)",
            "█".repeat(filled),
            "░".repeat(empty),
            if self.is_estimated { "~" } else { "" },
            total_k,
            ctx_window_k,
            self.utilization_pct,
        );
        lines.push(bar);
        lines.push(String::new());

        // Category breakdown
        lines.push("Estimated usage by category:".to_string());

        let categories = [
            ("System prompt", self.system_prompt_tokens),
            ("Tool definitions", self.tool_call_tokens),
            ("User messages", self.user_messages_tokens),
            ("Assistant msgs", self.assistant_messages_tokens),
            ("Tool results", self.tool_result_tokens),
            ("Thinking", self.thinking_tokens),
            ("Images/Files", self.image_tokens + self.file_tokens),
        ];

        for (name, tokens) in &categories {
            let pct = if self.context_window_size > 0 {
                (*tokens as f64 / self.context_window_size as f64) * 100.0
            } else {
                0.0
            };
            let tokens_display = format_token_k(*tokens);
            lines.push(format!(
                "  ⛶ {}: {} ({:.1}%)",
                name, tokens_display, pct
            ));
        }

        // Free space
        let free_pct = if self.context_window_size > 0 {
            (self.free_space as f64 / self.context_window_size as f64) * 100.0
        } else {
            0.0
        };
        lines.push(format!(
            "  ⛶ Free space: {} ({:.1}%)",
            free_k, free_pct
        ));

        lines.join("\n")
    }
}

/// Format token count as "N" or "N.k" for display.
fn format_token_k(count: u32) -> String {
    if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else if count > 0 {
        format!("{}", count)
    } else {
        "0".to_string()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    // `Message` is only referenced from these tests (the lib build no longer
    // imports it at module scope), so bring it in here to avoid an unused-import
    // warning in the non-test build.
    use crate::Message;

    #[test]
    fn test_context_accounting_empty_conversation() {
        let conv = Conversation::new();
        let accounting = ContextAccounting::account(&conv, "gpt-4o", 0);

        assert_eq!(accounting.system_prompt_tokens, 0);
        assert_eq!(accounting.user_messages_tokens, 0);
        assert_eq!(accounting.total_tokens, 0);
        assert_eq!(accounting.context_window_size, 200_000);
        assert_eq!(accounting.free_space, 200_000);
        assert!(accounting.is_estimated);
    }

    #[test]
    fn test_context_accounting_with_messages() {
        let mut conv = Conversation::new();
        conv.add_message(Message::system("You are a helpful assistant.".to_string()));
        conv.add_message(Message::user("What is Rust?".to_string()));
        conv.add_message(Message::assistant("Rust is a programming language.".to_string()));

        let accounting = ContextAccounting::account(&conv, "gpt-4o", 3);

        // Should have nonzero tokens for each category
        assert!(accounting.system_prompt_tokens > 0);
        assert!(accounting.user_messages_tokens > 0);
        assert!(accounting.assistant_messages_tokens > 0);
        assert!(accounting.tool_call_tokens > 0); // tool definition overhead for 3 tools
        assert!(accounting.total_tokens > 0);
        assert_eq!(accounting.context_window_size, 200_000);
        assert!(accounting.utilization_pct > 0.0);
        assert!(accounting.utilization_pct < 1.0);
    }

    #[test]
    fn test_context_accounting_with_tool_call() {
        let mut conv = Conversation::new();
        conv.add_message(Message::user("List files".to_string()));
        conv.add_message(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "I'll list the files.".to_string() },
                ContentBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "shell".to_string(),
                    args: "{\"command\":\"ls\"}".to_string(),
                },
            ],
            metadata: std::collections::HashMap::new(),
        });
        conv.add_message(Message::tool_result("call_1".to_string(), "file1.txt\nfile2.txt".to_string()));

        let accounting = ContextAccounting::account(&conv, "gpt-4o", 1);

        assert!(accounting.user_messages_tokens > 0);
        assert!(accounting.assistant_messages_tokens > 0);
        assert!(accounting.tool_call_tokens > 0);
        assert!(accounting.tool_result_tokens > 0);
    }

    #[test]
    fn test_context_accounting_format_display() {
        let mut conv = Conversation::new();
        conv.add_message(Message::system("You are helpful.".to_string()));
        conv.add_message(Message::user("Hello".to_string()));

        let accounting = ContextAccounting::account(&conv, "gpt-4o", 0);
        let display = accounting.format_display("OpenAI · gpt-4o");

        assert!(display.contains("📊 Context Usage"));
        assert!(display.contains("OpenAI · gpt-4o"));
        assert!(display.contains("System prompt"));
        assert!(display.contains("User messages"));
        assert!(display.contains("Free space"));
        // The bar contains ░ for empty portion; █ only appears with significant content
        assert!(display.contains("░"));
    }

    #[test]
    fn test_context_accounting_cjk_text() {
        let mut conv = Conversation::new();
        conv.add_message(Message::user("你好世界，这是一个中文测试".to_string()));

        // CJK text should use different chars-per-token ratio
        let accounting_openai = ContextAccounting::account(&conv, "gpt-4o", 0);
        let accounting_anthropic = ContextAccounting::account(&conv, "claude-sonnet-4-6-20250514", 0);

        // Both should count tokens, but with different ratios
        assert!(accounting_openai.user_messages_tokens > 0);
        assert!(accounting_anthropic.user_messages_tokens > 0);
    }

    #[test]
    fn test_context_accounting_thinking_content() {
        let mut conv = Conversation::new();
        conv.add_message(Message::user("Think about this".to_string()));
        conv.add_message(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking { text: "Let me reason about this step by step...".to_string() },
                ContentBlock::Text { text: "Here is my answer.".to_string() },
            ],
            metadata: std::collections::HashMap::new(),
        });

        let accounting = ContextAccounting::account(&conv, "claude-opus-4-8", 0);

        assert!(accounting.thinking_tokens > 0);
        assert!(accounting.assistant_messages_tokens > 0);
        assert_eq!(accounting.context_window_size, 200_000);
    }

    #[test]
    fn test_format_token_k() {
        assert_eq!(format_token_k(0), "0");
        assert_eq!(format_token_k(500), "500");
        assert_eq!(format_token_k(1000), "1.0k");
        assert_eq!(format_token_k(1500), "1.5k");
        assert_eq!(format_token_k(128000), "128.0k");
    }
}
