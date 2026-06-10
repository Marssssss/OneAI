//! Context compressor — summarizes older turns when threshold is exceeded.
//!
//! When the conversation context exceeds a token threshold, the compressor:
//! 1. Keeps the most recent N turns intact
//! 2. Summarizes older turns into a single compressed entry
//! 3. Uses an LLM provider for the summarization

use std::sync::Arc;

use oneai_core::{Conversation, InferenceRequest, Message, MemoryEntry, Role};
use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;

/// Context compressor that uses an LLM to summarize older conversation turns.
///
/// When the conversation exceeds a token threshold, the compressor keeps
/// the most recent turns intact and summarizes older ones into a single entry.
pub struct ContextCompressor {
    /// Token threshold for triggering compression.
    threshold_tokens: usize,
    /// Number of recent turns to keep intact.
    keep_recent_turns: usize,
    /// LLM provider for summarization.
    summarizer: Arc<dyn LlmProvider>,
}

impl ContextCompressor {
    /// Create a new compressor with the given settings and LLM provider.
    pub fn new(threshold_tokens: usize, keep_recent_turns: usize, summarizer: Arc<dyn LlmProvider>) -> Self {
        Self {
            threshold_tokens,
            keep_recent_turns,
            summarizer,
        }
    }

    /// Get the token threshold.
    pub fn threshold_tokens(&self) -> usize {
        self.threshold_tokens
    }

    /// Get the number of recent turns to keep.
    pub fn keep_recent_turns(&self) -> usize {
        self.keep_recent_turns
    }

    /// Estimate the token count of a conversation.
    ///
    /// Uses a rough heuristic: ~1 token per 4 characters of English text,
    /// plus overhead per message.
    pub fn estimate_tokens(conversation: &Conversation) -> usize {
        conversation.messages.iter().map(|msg| {
            msg.content.iter().map(|block| {
                match block {
                    oneai_core::ContentBlock::Text { text } => text.len() / 4 + 20,
                    oneai_core::ContentBlock::Image { .. } => 100, // Image tokens depend on size
                    oneai_core::ContentBlock::File { .. } => 50,
                    oneai_core::ContentBlock::ToolCall { name, args, .. } => name.len() / 4 + args.len() / 4 + 30,
                    oneai_core::ContentBlock::ToolResult { content, .. } => content.len() / 4 + 20,
                }
            }).sum::<usize>()
        }).sum()
    }

    /// Check if a conversation needs compression.
    pub fn needs_compression(&self, conversation: &Conversation) -> bool {
        Self::estimate_tokens(conversation) > self.threshold_tokens
    }

    /// Compress a conversation by summarizing older turns.
    ///
    /// Returns a new conversation where:
    /// - Recent turns (last `keep_recent_turns`) are kept intact
    /// - Older turns are replaced by a single summary message
    pub async fn compress(&self, conversation: &Conversation) -> Result<CompressedResult> {
        let total_messages = conversation.messages.len();
        if total_messages <= self.keep_recent_turns {
            return Ok(CompressedResult {
                compressed_conversation: conversation.clone(),
                summary: None,
                removed_entries: Vec::new(),
            });
        }

        // Split conversation: older turns to compress, recent turns to keep
        let split_point = total_messages - self.keep_recent_turns;
        let older_messages = &conversation.messages[..split_point];
        let recent_messages = &conversation.messages[split_point..];

        // Build the text to summarize
        let older_text = older_messages.iter()
            .map(|msg| {
                let role = match msg.role {
                    Role::System => "System",
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::Tool => "Tool",
                };
                format!("[{}]: {}", role, msg.text_content())
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Request summarization from the LLM
        let mut summary_conv = Conversation::new();
        summary_conv.add_message(Message::system(
            "You are a conversation summarizer. Summarize the conversation below \
            into a concise paragraph that captures the key facts, decisions, and \
            context needed to continue the conversation. Focus on information that \
            would be needed for follow-up questions. Be concise but complete."
        ));
        summary_conv.add_message(Message::user(format!(
            "Summarize this conversation:\n\n{}", older_text
        )));

        let request = InferenceRequest {
            conversation: summary_conv,
            tools: vec![],
            max_tokens: Some(512),
            temperature: Some(0.0),
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            metadata: std::collections::HashMap::new(),
        };

        let response = self.summarizer.infer(request).await?;
        let summary_text = response.message.text_content();

        // Build the compressed conversation
        let mut compressed = Conversation::with_id(conversation.id.clone());
        compressed.metadata = conversation.metadata.clone();

        // Add the summary as a system context message
        compressed.add_message(Message::system(
            "[Previous conversation summary]: ".to_string() + &summary_text
        ));

        // Add the recent turns intact
        for msg in recent_messages {
            compressed.add_message(msg.clone());
        }

        // Collect removed entries for potential long-term memory storage
        let removed_entries: Vec<MemoryEntry> = older_messages.iter()
            .enumerate()
            .map(|(i, msg)| {
                MemoryEntry {
                    id: format!("compressed_{}", i),
                    content: msg.text_content(),
                    timestamp: chrono::Utc::now(),
                    embedding: None,
                    metadata: std::collections::HashMap::from([
                        ("role".to_string(), match msg.role {
                            Role::System => "system".to_string(),
                            Role::User => "user".to_string(),
                            Role::Assistant => "assistant".to_string(),
                            Role::Tool => "tool".to_string(),
                        }),
                        ("compressed".to_string(), "true".to_string()),
                    ]),
                }
            })
            .collect();

        Ok(CompressedResult {
            compressed_conversation: compressed,
            summary: Some(summary_text),
            removed_entries,
        })
    }
}

/// Result of context compression.
#[derive(Debug, Clone)]
pub struct CompressedResult {
    /// The compressed conversation.
    pub compressed_conversation: Conversation,

    /// The generated summary (if compression was performed).
    pub summary: Option<String>,

    /// Entries that were removed during compression (for long-term memory storage).
    pub removed_entries: Vec<MemoryEntry>,
}