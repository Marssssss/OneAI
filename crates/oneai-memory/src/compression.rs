//! Context compressor — summarizes older turns when threshold is exceeded.
//!
//! When the conversation context exceeds a token threshold, the compressor:
//! 1. Keeps the most recent N turns intact
//! 2. Summarizes older turns into a single compressed entry
//! 3. Uses an LLM provider for the summarization
//!
//! Domain-specific behavior: when a CompressionTemplate is provided,
//! the summarization prompt follows the domain's preservation priorities,
//! producing structured summaries that preserve critical domain information.

use std::sync::Arc;

use oneai_core::{Conversation, InferenceRequest, Message, MemoryEntry, Role};
use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;
use oneai_core::budget::{ContextCompressorTrait, CompressedResult as CoreCompressedResult};

/// Context compressor that uses an LLM to summarize older conversation turns.
///
/// When the conversation exceeds a token threshold, the compressor keeps
/// the most recent turns intact and summarizes older ones into a single entry.
///
/// Implements `oneai_core::budget::ContextCompressorTrait` so it can be injected
/// into `ContextBudgetManager`, replacing the default `NoopCompressor`.
pub struct ContextCompressor {
    /// Token threshold for triggering compression.
    threshold_tokens: usize,
    /// Number of recent turns to keep intact.
    keep_recent_turns: usize,
    /// LLM provider for summarization.
    summarizer: Arc<dyn LlmProvider>,
    /// Domain-specific compression template (optional).
    compression_template: Option<oneai_domain::CompressionTemplate>,
    /// Optional fact extractor — runs over `discarded_messages` on each
    /// compression, turning summarized-away turns into durable archival facts
    /// (the "压缩即丢失" closure). None → discarded turns are not extracted.
    fact_extractor: Option<Arc<crate::fact_extraction::FactExtractor>>,
    /// Archival sink for extracted facts. Required when `fact_extractor` is set.
    archive: Option<Arc<crate::fact_store::MemoryFactStore>>,
    /// Namespace context for extracted facts.
    user_id: String,
    session_id: String,
}

impl ContextCompressor {
    /// Create a new compressor with the given settings and LLM provider.
    pub fn new(threshold_tokens: usize, keep_recent_turns: usize, summarizer: Arc<dyn LlmProvider>) -> Self {
        Self {
            threshold_tokens,
            keep_recent_turns,
            summarizer,
            compression_template: None,
            fact_extractor: None,
            archive: None,
            user_id: String::new(),
            session_id: String::new(),
        }
    }

    /// Create a compressor with a domain-specific compression template.
    pub fn with_template(
        threshold_tokens: usize,
        keep_recent_turns: usize,
        summarizer: Arc<dyn LlmProvider>,
        template: oneai_domain::CompressionTemplate,
    ) -> Self {
        Self {
            threshold_tokens,
            keep_recent_turns,
            summarizer,
            compression_template: Some(template),
            fact_extractor: None,
            archive: None,
            user_id: String::new(),
            session_id: String::new(),
        }
    }

    /// Enable compression-coupled fact extraction: on each compression, the
    /// discarded (summarized-away) turns are run through a `FactExtractor`
    /// guided by `schema`, and the resulting facts are conflict-resolved into
    /// `archive`. Reuses this compressor's LLM provider for extraction.
    pub fn with_fact_extraction(
        mut self,
        schema: Vec<oneai_core::FactType>,
        archive: Arc<crate::fact_store::MemoryFactStore>,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        self.fact_extractor = Some(Arc::new(crate::fact_extraction::FactExtractor::new(
            self.summarizer.clone(),
            schema,
        )));
        self.archive = Some(archive);
        self.user_id = user_id.into();
        self.session_id = session_id.into();
        self
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
                    oneai_core::ContentBlock::Thinking { text } => text.len() / 4 + 20,
                    _ => 50, // #[non_exhaustive] catch-all
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
                discarded_messages: Vec::new(),
            });
        }

        // Split conversation: older turns to compress, recent turns to keep.
        // The first user message is the original task — pin it verbatim (Q2
        // hard guarantee + the Q3 handoff must carry the original Goal) rather
        // than letting it fall into the summarizable segment and be summarized
        // away. It is pulled out of `older_messages` and re-added to the
        // compressed conversation intact, between the summary and the recent tail.
        let recent_start = total_messages - self.keep_recent_turns;
        let first_user_idx = conversation.messages.iter()
            .position(|m| m.role == Role::User);

        // The first user message is pinned only when it would otherwise be
        // summarized (i.e. it sits before the recent tail). When it's already
        // inside the recent tail it's kept verbatim by the recent-segment copy
        // below, so no special handling is needed.
        let pin_first_user = first_user_idx
            .map(|idx| idx < recent_start)
            .unwrap_or(false);

        // `older` = the summarizable segment = messages before the recent tail,
        // excluding the pinned first user message (if any). Owned because the
        // discarded segment is handed to the fact extractor + discarded sink.
        let older_indices: Vec<usize> = (0..recent_start)
            .filter(|&i| !(pin_first_user && Some(i) == first_user_idx))
            .collect();
        let older_messages: Vec<Message> = older_indices.iter()
            .map(|&i| conversation.messages[i].clone())
            .collect();
        let recent_messages = &conversation.messages[recent_start..];

        // Build the text to summarize.
        //
        // B4: cap each older message's contribution so a runaway tool_result
        // (long shell/file output) doesn't bloat the summarization prompt or
        // get summarized away wholesale. The capped view keeps a head + pointer
        // to `memory_search` for the full output —无损截断 before summary.
        const MAX_OLDER_MSG_CHARS: usize = 2000;
        let older_text = older_messages.iter()
            .map(|msg| {
                let role = match msg.role {
                    Role::System => "System",
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::Tool => "Tool",
                    _ => "User", // #[non_exhaustive] catch-all
                };
                let text = msg.text_content();
                let body = if text.chars().count() > MAX_OLDER_MSG_CHARS {
                    let head: String = text.chars().take(MAX_OLDER_MSG_CHARS).collect();
                    format!("{}\n[...content truncated — use memory_search for the full output]", head)
                } else {
                    text
                };
                format!("[{}]: {}", role, body)
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Determine summarization prompt — use domain template if present
        let task_desc = conversation.messages.iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.text_content())
            .unwrap_or_else(|| "unknown task".to_string());

        let summarization_prompt = if let Some(template) = &self.compression_template {
            template.build_summarization_prompt(&task_desc)
        } else {
            "You are a conversation summarizer. Summarize the conversation below \
            into a concise paragraph that captures the key facts, decisions, and \
            context needed to continue the conversation. Focus on information that \
            would be needed for follow-up questions. Be concise but complete.".to_string()
        };

        // Request summarization from the LLM
        let mut summary_conv = Conversation::new();
        summary_conv.add_message(Message::system(summarization_prompt));
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
            thinking_budget: None,
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

        // Pin the original task (first user message) verbatim — Q2/Q3 hard
        // guarantee. The model sees the unmodified goal alongside the handoff.
        if pin_first_user {
            if let Some(&idx) = first_user_idx.as_ref() {
                compressed.add_message(conversation.messages[idx].clone());
            }
        }

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
                            _ => "user".to_string(), // #[non_exhaustive] catch-all
                        }),
                        ("compressed".to_string(), "true".to_string()),
                    ]),
                }
            })
            .collect();

        // Compression-coupled fact extraction: turn the discarded (summarized-away)
        // turns into durable archival facts before they're lost. Fail-safe.
        self.extract_and_archive(&older_messages).await;

        Ok(CompressedResult {
            compressed_conversation: compressed,
            summary: Some(summary_text),
            removed_entries,
            discarded_messages: older_messages,
        })
    }

    /// Run the compression-coupled fact extractor over discarded messages and
    /// archive the results. Fail-safe: extraction errors are logged and never
    /// propagate — a bad extraction must not break the compression path.
    async fn extract_and_archive(&self, discarded: &[Message]) {
        if discarded.is_empty() {
            return;
        }
        let (extractor, archive) = match (&self.fact_extractor, &self.archive) {
            (Some(ext), Some(arch)) => (ext.clone(), arch.clone()),
            _ => return, // extraction not configured
        };
        match extractor.extract(discarded, &self.user_id, &self.session_id).await {
            Ok(facts) => {
                if !facts.is_empty() {
                    tracing::debug!(
                        fact_count = facts.len(),
                        "archived facts extracted from {} discarded messages",
                        discarded.len()
                    );
                    for fact in facts {
                        archive.upsert(fact).await;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("fact extraction failed (compression proceeds, facts not archived): {}", e);
            }
        }
    }
}

// ─── Implement core ContextCompressorTrait ────────────────────────────────────────

/// Bridge between oneai-memory::ContextCompressor and oneai_core::budget::ContextCompressorTrait.
///
/// This allows the real ContextCompressor (with domain-specific CompressionTemplate)
/// to be injected into ContextBudgetManager, replacing the default NoopCompressor.
#[async_trait::async_trait]
impl ContextCompressorTrait for ContextCompressor {
    fn estimate_tokens(&self, conversation: &Conversation) -> usize {
        Self::estimate_tokens(conversation)
    }

    fn estimate_tokens_of_message(&self, msg: &Message) -> usize {
        msg.content.iter()
            .filter_map(|block| match block {
                oneai_core::ContentBlock::Text { text } => Some(text.len()),
                _ => Some(50),
            })
            .sum::<usize>() / 4 + 20  // overhead per message
    }

    async fn compress(&self, conversation: &Conversation) -> Result<CoreCompressedResult> {
        let result = self.compress(conversation).await?;
        Ok(CoreCompressedResult {
            compressed_conversation: result.compressed_conversation,
            summary: result.summary,
            discarded_messages: result.discarded_messages,
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

    /// The original messages that were summarized away (the "older" segment).
    ///
    /// Fed to the optional `FactExtractor` so compressed-away content becomes
    /// durable archival facts instead of being lost.
    pub discarded_messages: Vec<Message>,
}
#[cfg(test)]
mod closure_tests {
    use super::*;
    use oneai_core::{FactType, InferenceRequest, InferenceResponse, Message, ModelCapability, ModelConfig, ProviderType, Role, TokenUsage};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Mock provider: returns a fact JSON when the prompt looks like an
    /// extraction request, otherwise returns a short summary.
    struct DualMockProvider;
    #[async_trait::async_trait]
    impl LlmProvider for DualMockProvider {
        async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse> {
            let user_text = req.conversation.messages.iter()
                .filter(|m| m.role == Role::System)
                .map(|m| m.text_content())
                .collect::<Vec<_>>().join(" ");
            let body = if user_text.contains("memory extractor") {
                r#"[{"fact_type":"user_tooling_pref","subject":"user.package_manager","predicate":"prefers","content":"pnpm"}]"#.to_string()
            } else {
                "summarized: user prefers pnpm.".to_string()
            };
            Ok(InferenceResponse {
                message: Message::assistant(body),
                usage: TokenUsage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0, ..Default::default()},
                model: "dual-mock".to_string(),
                metadata: HashMap::new(),
            })
        }
        async fn infer_stream(
            &self, _req: InferenceRequest,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = oneai_core::InferenceStreamChunk> + Send>>> {
            Err(oneai_core::error::OneAIError::Provider("no stream".into()))
        }
        fn capabilities(&self) -> ModelCapability {
            ModelCapability { supports_multimodal: false, supports_streaming: false, supports_tools: false, context_window_size: 4096, max_output_tokens: 512 }
        }
        fn config(&self) -> &ModelConfig {
            static CONFIG: std::sync::OnceLock<ModelConfig> = std::sync::OnceLock::new();
            CONFIG.get_or_init(|| ModelConfig { provider_type: ProviderType::Local, cloud_kind: None, api_key: None, base_url: None, port: None, model_name: Some("dual-mock".into()), model_path: None, ..Default::default() })
        }
    }

    fn long_conversation() -> Conversation {
        // Enough turns to exceed keep_recent_turns so compression discards some.
        let mut conv = Conversation::new();
        conv.add_message(Message::user("I use pnpm for package management."));
        for i in 0..12 {
            conv.add_message(Message::assistant(format!("ack {}", i)));
            conv.add_message(Message::user(format!("turn {}", i)));
        }
        conv
    }

    #[tokio::test]
    async fn compression_archives_extracted_facts_from_discarded_turns() {
        let archive = Arc::new(crate::fact_store::MemoryFactStore::new());
        let compressor = ContextCompressor::new(1, 6, Arc::new(DualMockProvider))
            .with_fact_extraction(
                vec![FactType::new("user_tooling_pref")],
                archive.clone(),
                "alice",
                "s1",
            );

        let result = compressor.compress(&long_conversation()).await.unwrap();
        // The discarded segment was non-empty and carried out.
        assert!(!result.discarded_messages.is_empty());
        // And its content was extracted + archived (not lost).
        let facts = archive.all().await;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "user.package_manager");
        assert_eq!(facts[0].content, "pnpm");
        assert_eq!(facts[0].user_id, "alice");
    }

    #[tokio::test]
    async fn compression_without_extraction_does_not_archive() {
        let archive = Arc::new(crate::fact_store::MemoryFactStore::new());
        // No with_fact_extraction → no archival side-effect.
        let compressor = ContextCompressor::new(1, 6, Arc::new(DualMockProvider));
        let _ = compressor.compress(&long_conversation()).await.unwrap();
        assert!(archive.all().await.is_empty());
    }

    #[tokio::test]
    async fn compression_preserves_first_user_message_and_metadata() {
        // Q2 hard guarantee: the original task (first user message) survives
        // compression verbatim instead of being summarized away. Metadata
        // (task_anchor / plan_state) is also copied through.
        let compressor = ContextCompressor::new(1, 6, Arc::new(DualMockProvider));
        let mut conv = long_conversation();
        conv.metadata.insert("task_anchor".to_string(), "I use pnpm".to_string());
        let result = compressor.compress(&conv).await.unwrap();

        // The first user message text appears verbatim in the compressed conv.
        let compressed_text: String = result.compressed_conversation.messages.iter()
            .map(|m| m.text_content()).collect::<Vec<_>>().join("\n");
        assert!(compressed_text.contains("I use pnpm for package management."),
            "first user message must be pinned verbatim, got: {compressed_text}");
        // Metadata carried through.
        assert_eq!(
            result.compressed_conversation.metadata.get("task_anchor"),
            Some(&"I use pnpm".to_string()),
        );
        // The pinned first user message is NOT in the discarded segment.
        assert!(!result.discarded_messages.iter().any(|m| m.role == Role::User
            && m.text_content().contains("I use pnpm for package management.")));
    }
}
