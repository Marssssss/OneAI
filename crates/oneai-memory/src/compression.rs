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

use oneai_core::budget::{CompressedResult as CoreCompressedResult, ContextCompressorTrait};
use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;
use oneai_core::{Conversation, InferenceRequest, MemoryEntry, Message, Role};

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
    /// Routed through `MemoryManager::archive_facts` (embed + persist) so
    /// extracted facts are semantically recallable and survive restart.
    fact_sink: Option<Arc<dyn crate::FactSink>>,
    /// Namespace context for extracted facts.
    user_id: String,
    session_id: String,
}

impl ContextCompressor {
    /// Default number of recent *messages* to keep verbatim during compression.
    ///
    /// Agentic loops emit ~2 messages per tool round-trip (ToolCall +
    /// ToolResult), so 16 messages ≈ 8 tool round-trips of headroom beyond the
    /// latest user instruction — which is *always* kept verbatim regardless of
    /// this value (see `compress`'s latest-user extension). The previous
    /// default of 6 let a steering instruction fall out of the tail within ~3
    /// tool round-trips. Override via the `ContextCompressor::new` /
    /// `with_template` constructor (programmatic) or the `/compact` parameter
    /// (CLI); raise for long-context models, lower only when a domain needs
    /// more aggressive summarization.
    pub const DEFAULT_KEEP_RECENT_TURNS: usize = 16;

    /// Create a new compressor with the given settings and LLM provider.
    pub fn new(
        threshold_tokens: usize,
        keep_recent_turns: usize,
        summarizer: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            threshold_tokens,
            keep_recent_turns,
            summarizer,
            compression_template: None,
            fact_extractor: None,
            fact_sink: None,
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
            fact_sink: None,
            user_id: String::new(),
            session_id: String::new(),
        }
    }

    /// Enable compression-coupled fact extraction: on each compression, the
    /// discarded (summarized-away) turns are run through a `FactExtractor`
    /// guided by `schema`, and the resulting facts are routed through the
    /// `FactSink` (the canonical embed → upsert → persist path) so they are
    /// semantically recallable and survive restart. Reuses this compressor's
    /// LLM provider for extraction.
    pub fn with_fact_extraction(
        mut self,
        schema: Vec<oneai_core::FactType>,
        fact_sink: Arc<dyn crate::FactSink>,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        self.fact_extractor = Some(Arc::new(crate::fact_extraction::FactExtractor::new(
            self.summarizer.clone(),
            schema,
        )));
        self.fact_sink = Some(fact_sink);
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
        conversation
            .messages
            .iter()
            .map(|msg| {
                msg.content
                    .iter()
                    .map(|block| {
                        match block {
                            oneai_core::ContentBlock::Text { text } => text.len() / 4 + 20,
                            oneai_core::ContentBlock::Image { .. } => 100, // Image tokens depend on size
                            oneai_core::ContentBlock::File { .. } => 50,
                            oneai_core::ContentBlock::ToolCall { name, args, .. } => {
                                name.len() / 4 + args.len() / 4 + 30
                            }
                            oneai_core::ContentBlock::ToolResult { content, .. } => {
                                content.len() / 4 + 20
                            }
                            oneai_core::ContentBlock::Thinking { text } => text.len() / 4 + 20,
                            _ => 50, // #[non_exhaustive] catch-all
                        }
                    })
                    .sum::<usize>()
            })
            .sum()
    }

    /// Check if a conversation needs compression.
    pub fn needs_compression(&self, conversation: &Conversation) -> bool {
        Self::estimate_tokens(conversation) > self.threshold_tokens
    }

    /// Compress a conversation by summarizing older turns.
    ///
    /// Returns a new conversation where:
    /// - Leading system messages (the base prompt) are pinned verbatim FIRST
    /// - A single summary message follows the base prompt
    /// - The first user message (original task) is pinned verbatim
    /// - Recent turns (last `keep_recent_turns`) are kept intact
    ///
    /// Idempotence (2026-09 webUI verify-session incident): messages that are
    /// NOT removable — the base prompt and any previous summary — never count
    /// as "older turns". When nothing else sits before the recent tail, the
    /// conversation is returned unchanged and NO LLM call is made. Previously
    /// the previous summary itself was the only "older" message and got
    /// re-summarized every iteration: zero net shrinkage, lossy churn, and
    /// non-deterministic wording that pinned the provider's prompt-prefix
    /// cache at ~2.5% hit rate.
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
        let recent_start_default = total_messages - self.keep_recent_turns;
        let first_user_idx = conversation
            .messages
            .iter()
            .position(|m| m.role == Role::User);

        // Extend the recent tail backward to include the *most recent* user
        // message verbatim — symmetric to the first-user pin. In an agentic
        // turn a single user instruction is followed by many tool round-trips
        // (each ToolCall + ToolResult = 2 messages), so a message-count window
        // pushes the user's steering instruction out of the tail within ~3 tool
        // round-trips and summarizes it into the lossy summary. The model then
        // re-derives its objective from the pinned *original* task and
        // re-analyzes from scratch. Extending `recent_start` to the latest user
        // index guarantees the live instruction always survives in-context.
        let last_user_idx = conversation
            .messages
            .iter()
            .rposition(|m| m.role == Role::User);
        let recent_start = match last_user_idx {
            Some(idx) if idx < recent_start_default => idx,
            _ => recent_start_default,
        };

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
        // Leading system messages before the first user message = the base
        // prompt (+ any session-level system context). Pinned verbatim, never
        // summarized — the incident's FIRST compression discarded exactly the
        // base prompt, losing the foundational instructions (2026-09 webUI
        // verify-session).
        let leading_system_end = first_user_idx.unwrap_or(total_messages);
        // NOTE: a summary message is NEVER "protected" even when it sits in
        // the leading region (after a compression it lives between the base
        // prompt and the pinned first user) — it takes the carryover path.
        let is_protected_system = |i: usize| {
            i < leading_system_end
                && conversation.messages[i].role == Role::System
                && !oneai_core::context_manager::is_summary_message(&conversation.messages[i])
        };

        // `older` = the summarizable segment = messages before the recent
        // tail, excluding: (a) the pinned first user message, (b) protected
        // leading system messages, (c) any previous summary message — a
        // summary is the compressed form of everything summarized before;
        // re-summarizing it alone is lossy churn. The LATEST previous
        // summary's text is folded into the NEW summary's input instead, so
        // the conversation keeps exactly one summary. Owned because the
        // discarded segment is handed to the fact extractor + discarded sink.
        let mut carryover_summary: Option<String> = None;
        let mut older_messages: Vec<Message> = Vec::new();
        for i in 0..recent_start {
            if is_protected_system(i) || (pin_first_user && Some(i) == first_user_idx) {
                continue;
            }
            let msg = &conversation.messages[i];
            if oneai_core::context_manager::is_summary_message(msg) {
                carryover_summary = Some(
                    msg.text_content()
                        [oneai_core::context_manager::PREV_CONVERSATION_SUMMARY_PREFIX.len()..]
                        .to_string(),
                );
            } else {
                older_messages.push(msg.clone());
            }
        }
        let recent_messages = &conversation.messages[recent_start..];

        // Idempotent short-circuit — nothing removable before the recent tail
        // (only the base prompt and/or a previous summary; or a single long
        // agentic turn whose latest user instruction forced `recent_start`
        // back to 0). The 无损截断 tier (budget.rs Step 2) already capped
        // tool results before us; summarization can't help further. Return
        // as-is with NO LLM call — this kills the incident's
        // re-summarize-the-previous-summary-every-iteration loop (zero net
        // shrinkage + cache-destroying wording churn).
        if older_messages.is_empty() {
            return Ok(CompressedResult {
                compressed_conversation: conversation.clone(),
                summary: None,
                removed_entries: Vec::new(),
                discarded_messages: Vec::new(),
            });
        }

        // Build the text to summarize.
        //
        // B4: cap each older message's contribution so a runaway tool_result
        // (long shell/file output) doesn't bloat the summarization prompt or
        // get summarized away wholesale. The capped view keeps a head + pointer
        // to `memory_search` for the full output —无损截断 before summary.
        const MAX_OLDER_MSG_CHARS: usize = 2000;
        let older_text = older_messages
            .iter()
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
                    format!(
                        "{}\n[...content truncated — use memory_search for the full output]",
                        head
                    )
                } else {
                    text
                };
                format!("[{}]: {}", role, body)
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Fold the previous summary (if any) into the new one — the output
        // keeps exactly ONE summary covering everything summarized so far.
        let older_text = if let Some(prev) = carryover_summary {
            format!("[Previous summary]: {}\n{}", prev, older_text)
        } else {
            older_text
        };

        // Determine summarization prompt — use domain template if present
        let task_desc = conversation
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.text_content())
            .unwrap_or_else(|| "unknown task".to_string());

        let summarization_prompt = if let Some(template) = &self.compression_template {
            template.build_summarization_prompt(&task_desc)
        } else {
            "You are a conversation summarizer. Summarize the conversation below \
            into a concise paragraph that captures the key facts, decisions, and \
            context needed to continue the conversation. Focus on information that \
            would be needed for follow-up questions. Be concise but complete."
                .to_string()
        };

        // Request summarization from the LLM
        let mut summary_conv = Conversation::new();
        summary_conv.add_message(Message::system(summarization_prompt));
        summary_conv.add_message(Message::user(format!(
            "Summarize this conversation:\n\n{}",
            older_text
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

        // Base prompt FIRST — it must keep anchoring the provider's
        // prompt-prefix cache, and the model must keep seeing its foundational
        // instructions (the incident's first compression replaced the base
        // prompt with the summary and the instructions were lost).
        for (idx, msg) in conversation.messages.iter().enumerate() {
            if is_protected_system(idx) {
                compressed.add_message(msg.clone());
            }
        }

        // Add the summary AFTER the base prompt as a system context message
        compressed.add_message(Message::system(
            oneai_core::context_manager::PREV_CONVERSATION_SUMMARY_PREFIX.to_string()
                + &summary_text,
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
        let removed_entries: Vec<MemoryEntry> = older_messages
            .iter()
            .enumerate()
            .map(|(i, msg)| {
                MemoryEntry {
                    id: format!("compressed_{}", i),
                    content: msg.text_content(),
                    timestamp: chrono::Utc::now(),
                    embedding: None,
                    metadata: std::collections::HashMap::from([
                        (
                            "role".to_string(),
                            match msg.role {
                                Role::System => "system".to_string(),
                                Role::User => "user".to_string(),
                                Role::Assistant => "assistant".to_string(),
                                Role::Tool => "tool".to_string(),
                                _ => "user".to_string(), // #[non_exhaustive] catch-all
                            },
                        ),
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
        let (extractor, sink) = match (&self.fact_extractor, &self.fact_sink) {
            (Some(ext), Some(sink)) => (ext.clone(), sink.clone()),
            _ => return, // extraction not configured
        };
        match extractor
            .extract(discarded, &self.user_id, &self.session_id)
            .await
        {
            Ok(facts) => {
                if !facts.is_empty() {
                    tracing::debug!(
                        fact_count = facts.len(),
                        "archived facts extracted from {} discarded messages",
                        discarded.len()
                    );
                    // Route through the FactSink (MemoryManager::archive_facts)
                    // so extracted facts are embedded for semantic recall and
                    // durably persisted — NOT raw-upserted (which left them
                    // un-embedded, un-persisted, and invisible to semantic
                    // recall after restart). §12.1.
                    sink.archive_facts(facts).await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    "fact extraction failed (compression proceeds, facts not archived): {}",
                    e
                );
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
        msg.content
            .iter()
            .map(|block| match block {
                oneai_core::ContentBlock::Text { text } => text.len(),
                _ => 50,
            })
            .sum::<usize>()
            / 4
            + 20 // overhead per message
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
    use oneai_core::{
        FactType, InferenceRequest, InferenceResponse, Message, ModelCapability, ModelConfig,
        ProviderType, Role, TokenUsage,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Mock provider: returns a fact JSON when the prompt looks like an
    /// extraction request, otherwise returns a short summary.
    struct DualMockProvider;
    #[async_trait::async_trait]
    impl LlmProvider for DualMockProvider {
        async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse> {
            let user_text = req
                .conversation
                .messages
                .iter()
                .filter(|m| m.role == Role::System)
                .map(|m| m.text_content())
                .collect::<Vec<_>>()
                .join(" ");
            let body = if user_text.contains("memory extractor") {
                r#"[{"fact_type":"user_tooling_pref","subject":"user.package_manager","predicate":"prefers","content":"pnpm"}]"#.to_string()
            } else {
                "summarized: user prefers pnpm.".to_string()
            };
            Ok(InferenceResponse {
                message: Message::assistant(body),
                usage: TokenUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    ..Default::default()
                },
                model: "dual-mock".to_string(),
                metadata: HashMap::new(),
            })
        }
        async fn infer_stream(
            &self,
            _req: InferenceRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = oneai_core::InferenceStreamChunk> + Send>>,
        > {
            Err(oneai_core::error::OneAIError::Provider("no stream".into()))
        }
        fn capabilities(&self) -> ModelCapability {
            ModelCapability {
                supports_multimodal: false,
                supports_streaming: false,
                supports_tools: false,
                context_window_size: 4096,
                max_output_tokens: 512,
            }
        }
        fn config(&self) -> &ModelConfig {
            static CONFIG: std::sync::OnceLock<ModelConfig> = std::sync::OnceLock::new();
            CONFIG.get_or_init(|| ModelConfig {
                provider_type: ProviderType::Local,
                cloud_kind: None,
                api_key: None,
                base_url: None,
                port: None,
                model_name: Some("dual-mock".into()),
                model_path: None,
                ..Default::default()
            })
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
        // Route through a real MemoryManager so the extracted facts traverse
        // the canonical embed → upsert → persist path (FactSink), matching
        // production — not a raw FactStore upsert that left facts un-embedded.
        let manager = Arc::new(crate::manager::MemoryManager::new());
        let compressor = ContextCompressor::new(1, 6, Arc::new(DualMockProvider))
            .with_fact_extraction(
                vec![FactType::new("user_tooling_pref")],
                manager.clone(),
                "alice",
                "s1",
            );

        let result = compressor.compress(&long_conversation()).await.unwrap();
        // The discarded segment was non-empty and carried out.
        assert!(!result.discarded_messages.is_empty());
        // And its content was extracted + archived (not lost).
        let facts = manager.fact_archive().all().await;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "user.package_manager");
        assert_eq!(facts[0].content, "pnpm");
        assert_eq!(facts[0].user_id, "alice");
    }

    #[tokio::test]
    async fn compression_without_extraction_does_not_archive() {
        let manager = Arc::new(crate::manager::MemoryManager::new());
        // No with_fact_extraction → no archival side-effect.
        let compressor = ContextCompressor::new(1, 6, Arc::new(DualMockProvider));
        let _ = compressor.compress(&long_conversation()).await.unwrap();
        assert!(manager.fact_archive().all().await.is_empty());
    }

    #[tokio::test]
    async fn compression_preserves_first_user_message_and_metadata() {
        // Q2 hard guarantee: the original task (first user message) survives
        // compression verbatim instead of being summarized away. Metadata
        // (task_anchor / plan_state) is also copied through.
        let compressor = ContextCompressor::new(1, 6, Arc::new(DualMockProvider));
        let mut conv = long_conversation();
        conv.metadata
            .insert("task_anchor".to_string(), "I use pnpm".to_string());
        let result = compressor.compress(&conv).await.unwrap();

        // The first user message text appears verbatim in the compressed conv.
        let compressed_text: String = result
            .compressed_conversation
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            compressed_text.contains("I use pnpm for package management."),
            "first user message must be pinned verbatim, got: {compressed_text}"
        );
        // Metadata carried through.
        assert_eq!(
            result.compressed_conversation.metadata.get("task_anchor"),
            Some(&"I use pnpm".to_string()),
        );
        // The pinned first user message is NOT in the discarded segment.
        assert!(!result
            .discarded_messages
            .iter()
            .any(|m| m.role == Role::User
                && m.text_content()
                    .contains("I use pnpm for package management.")));
    }

    #[tokio::test]
    async fn compression_keeps_latest_user_message_in_recent_tail() {
        // Regression for the "re-analyze from scratch" bug: in an agentic turn
        // the latest user steering instruction is followed by many tool
        // round-trips (each ≥2 messages). A message-count `keep_recent` window
        // pushed that instruction out of the tail within ~3 tool round-trips
        // and summarized it away — the model then re-derived its objective
        // from the pinned *original* task and re-analyzed. The recent tail must
        // extend back to include the most recent user message verbatim.
        let compressor = ContextCompressor::new(1, 6, Arc::new(DualMockProvider));
        let mut conv = Conversation::new();
        conv.add_message(Message::user("original task")); // first user — pinned
                                                          // Older filler turns — these get summarized away.
        conv.add_message(Message::assistant("ack 1"));
        conv.add_message(Message::assistant("ack 2"));
        conv.add_message(Message::assistant("ack 3"));
        // Latest user steering instruction — MUST survive verbatim.
        conv.add_message(Message::user("START FROM P0 NOW (latest instruction)"));
        // Tool round-trips after the latest user push it past a 6-message tail.
        for i in 0..8 {
            conv.add_message(Message::assistant(format!("toolcall {}", i)));
            conv.add_message(Message::assistant(format!("toolresult {}", i)));
        }
        // total = 21; recent_start_default = 21-6 = 15; last_user_idx = 4 < 15
        // → tail extends back to 4, keeping the latest user verbatim.
        let result = compressor.compress(&conv).await.unwrap();

        let compressed_text: String = result
            .compressed_conversation
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            compressed_text.contains("START FROM P0 NOW (latest instruction)"),
            "latest user instruction must survive in the recent tail, got: {compressed_text}"
        );
        assert!(
            !result
                .discarded_messages
                .iter()
                .any(|m| m.text_content().contains("START FROM P0 NOW")),
            "latest user instruction must NOT be summarized into the discarded segment"
        );
    }

    #[tokio::test]
    async fn compression_no_summary_when_only_recent_tail_remains() {
        // A single long agentic turn (one user message + many tool
        // round-trips): the latest-user extension pulls `recent_start` back to
        // 0, leaving the older segment empty. There is nothing to summarize,
        // so the compressor returns the conversation as-is — no LLM call.
        let compressor = ContextCompressor::new(1, 6, Arc::new(DualMockProvider));
        let mut conv = Conversation::new();
        conv.add_message(Message::user("the only user instruction"));
        for i in 0..10 {
            conv.add_message(Message::assistant(format!("toolcall {}", i)));
            conv.add_message(Message::assistant(format!("toolresult {}", i)));
        }
        let total = conv.messages.len();
        let result = compressor.compress(&conv).await.unwrap();
        assert!(
            result.summary.is_none(),
            "no summary expected when the older segment is empty"
        );
        assert!(result.discarded_messages.is_empty());
        assert_eq!(
            result.compressed_conversation.messages.len(),
            total,
            "conversation preserved verbatim — nothing to compress"
        );
    }

    /// Deterministic 4-d embedder for the routing regression below.
    struct StubEmbedder;
    #[async_trait::async_trait]
    impl oneai_core::traits::EmbeddingService for StubEmbedder {
        async fn embed(&self, text: &str) -> oneai_core::error::Result<Vec<f32>> {
            Ok(vec![text.len() as f32, 1.0, 0.0, 0.0])
        }
        async fn embed_batch(&self, texts: &[String]) -> oneai_core::error::Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }
        fn model(&self) -> oneai_core::traits::EmbeddingModel {
            oneai_core::traits::EmbeddingModel::new("stub-4d")
        }
        fn dimension(&self) -> usize {
            4
        }
    }

    /// Provider that counts `infer` calls and captures every system prompt —
    /// for the idempotence + carryover regressions below.
    #[derive(Default)]
    struct CountingProvider {
        calls: std::sync::atomic::AtomicUsize,
        prompts: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for CountingProvider {
        async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Capture ALL messages — the summarized conversation rides in a
            // user message, the summarization instructions in a system one.
            let full = req
                .conversation
                .messages
                .iter()
                .map(|m| m.text_content())
                .collect::<Vec<_>>()
                .join(" ");
            self.prompts.lock().unwrap().push(full);
            Ok(InferenceResponse {
                message: Message::assistant("fresh summary".to_string()),
                usage: TokenUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    ..Default::default()
                },
                model: "counting-mock".to_string(),
                metadata: HashMap::new(),
            })
        }
        async fn infer_stream(
            &self,
            _req: InferenceRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = oneai_core::InferenceStreamChunk> + Send>>,
        > {
            Err(oneai_core::error::OneAIError::Provider("no stream".into()))
        }
        fn capabilities(&self) -> ModelCapability {
            ModelCapability {
                supports_multimodal: false,
                supports_streaming: false,
                supports_tools: false,
                context_window_size: 4096,
                max_output_tokens: 512,
            }
        }
        fn config(&self) -> &ModelConfig {
            static CONFIG: std::sync::OnceLock<ModelConfig> = std::sync::OnceLock::new();
            CONFIG.get_or_init(|| ModelConfig {
                provider_type: ProviderType::Local,
                cloud_kind: None,
                api_key: None,
                base_url: None,
                port: None,
                model_name: Some("counting-mock".into()),
                model_path: None,
                ..Default::default()
            })
        }
    }

    /// base prompt + first user task + many alternating filler turns (enough
    /// to overflow keep_recent_turns = 6 with genuinely removable older
    /// content; the LAST user message sits inside the default recent tail, so
    /// the latest-user extension does not pull the tail back to index 1).
    fn base_plus_long_multi_turn() -> Conversation {
        let mut conv = Conversation::new();
        conv.add_message(Message::system("BASE PROMPT — foundational rules"));
        conv.add_message(Message::user("the original task"));
        for i in 0..12 {
            conv.add_message(Message::assistant(format!("ack {}", i)));
            conv.add_message(Message::user(format!("turn {}", i)));
        }
        conv
    }

    #[tokio::test]
    async fn compression_pins_base_prompt_and_puts_summary_after_it() {
        // Incident (2026-09 verify-session): the FIRST compression summarized
        // the base prompt away — the foundational instructions vanished and
        // the summary replaced the conversation's system head. Now the base
        // prompt is pinned verbatim at index 0, the summary sits AFTER it.
        let provider = Arc::new(CountingProvider::default());
        let compressor = ContextCompressor::new(1, 6, provider.clone());
        let result = compressor
            .compress(&base_plus_long_multi_turn())
            .await
            .unwrap();

        assert!(result.summary.is_some(), "real compression expected");
        let msgs = &result.compressed_conversation.messages;
        assert_eq!(msgs[0].role, Role::System);
        assert!(
            msgs[0].text_content().contains("BASE PROMPT"),
            "base prompt must be pinned verbatim first, got: {}",
            msgs[0].text_content()
        );
        assert!(
            msgs[1]
                .text_content()
                .starts_with(oneai_core::context_manager::PREV_CONVERSATION_SUMMARY_PREFIX),
            "summary must follow the base prompt, got: {}",
            msgs[1].text_content()
        );
        // The base prompt is NOT part of the discarded segment.
        assert!(
            !result
                .discarded_messages
                .iter()
                .any(|m| m.text_content().contains("BASE PROMPT")),
            "base prompt must never be discarded/fact-extracted"
        );
    }

    #[tokio::test]
    async fn compression_is_idempotent_when_only_base_and_summary_precede_tail() {
        // The incident's steady state: [base prompt, previous summary, user,
        // recent tail]. Nothing before the tail is removable, yet the old
        // code re-summarized the previous summary EVERY iteration — zero net
        // shrinkage, lossy churn, cache-destroying wording drift. Now: no LLM
        // call, conversation returned unchanged.
        let provider = Arc::new(CountingProvider::default());
        let compressor = ContextCompressor::new(1, 6, provider.clone());
        let mut conv = Conversation::new();
        conv.add_message(Message::system("BASE PROMPT — foundational rules"));
        conv.add_message(Message::system(
            "[Previous conversation summary]: earlier work distilled",
        ));
        conv.add_message(Message::user("the original task"));
        for i in 0..20 {
            conv.add_message(Message::assistant(format!("work step {}", i)));
        }
        // total = 23 > keep 6 → compression is "needed", but nothing is removable.
        let result = compressor.compress(&conv).await.unwrap();
        assert!(
            result.summary.is_none(),
            "no new summary when nothing removable precedes the tail"
        );
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "idempotent short-circuit must not call the LLM"
        );
        assert_eq!(
            result.compressed_conversation.messages.len(),
            conv.messages.len(),
            "conversation preserved verbatim"
        );
    }

    #[tokio::test]
    async fn compression_folds_previous_summary_into_new_one() {
        // When genuine older turns ARE removable and a previous summary
        // exists, the previous summary's text is folded into the NEW
        // summary's input (one summary total) and it is not re-summarized
        // alone or discarded.
        let provider = Arc::new(CountingProvider::default());
        let compressor = ContextCompressor::new(1, 6, provider.clone());
        let mut conv = Conversation::new();
        conv.add_message(Message::system("BASE PROMPT"));
        conv.add_message(Message::system(
            "[Previous conversation summary]: OLD SUMMARY CONTENT",
        ));
        conv.add_message(Message::user("original task"));
        for i in 0..8 {
            conv.add_message(Message::assistant(format!("early step {}", i)));
        }
        conv.add_message(Message::user("steer now"));
        for i in 0..14 {
            conv.add_message(Message::assistant(format!("late step {}", i)));
        }
        // total = 26, keep = 6 → recent_start_default = 20; last_user = 11 < 20
        // → recent_start = 11; older = [base(protected), summary(carryover),
        // original user(pinned), early steps ×8] → 8 removable messages.
        let result = compressor.compress(&conv).await.unwrap();
        assert!(result.summary.is_some());
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let prompt = provider.prompts.lock().unwrap()[0].clone();
        assert!(
            prompt.contains("OLD SUMMARY CONTENT"),
            "previous summary must be folded into the new summarization input"
        );
        // Output carries exactly ONE summary and the pinned base prompt.
        let msgs = &result.compressed_conversation.messages;
        let summary_count = msgs
            .iter()
            .filter(|m| {
                oneai_core::context_manager::is_summary_message(m)
                    && m.text_content().contains("fresh summary")
            })
            .count();
        assert_eq!(summary_count, 1);
        assert!(msgs[0].text_content().contains("BASE PROMPT"));
        // Neither the base prompt nor the old summary is in the discarded set.
        assert!(!result
            .discarded_messages
            .iter()
            .any(|m| m.text_content().contains("OLD SUMMARY CONTENT")));
        // The genuinely removed turns ARE discarded (fact extraction input):
        // the 8 early steps — base/summary/pinned-first-user all excluded.
        assert_eq!(result.discarded_messages.len(), 8);
    }

    #[tokio::test]
    async fn compression_routes_extracted_facts_through_embed_and_persist() {
        // §12.1 regression: extracted facts must flow through the FactSink
        // (MemoryManager::archive_facts → embed_fact), NOT a raw
        // MemoryFactStore::upsert that left embedding: None (invisible to
        // semantic recall + lost on restart). This is the single most severe
        // memory gap; this test pins the fix.
        let mm = Arc::new(crate::manager::MemoryManager::with_embedding(
            crate::manager::MemoryManagerConfig::default(),
            Arc::new(StubEmbedder),
        ));
        let compressor = ContextCompressor::new(1, 6, Arc::new(DualMockProvider))
            .with_fact_extraction(
                vec![FactType::new("user_tooling_pref")],
                mm.clone(),
                "alice",
                "s1",
            );
        let _ = compressor.compress(&long_conversation()).await.unwrap();
        let facts = mm.fact_archive().all().await;
        assert_eq!(facts.len(), 1);
        assert!(
            facts[0].embedding.is_some(),
            "extracted fact must be embedded via FactSink → archive_facts, not raw-upserted"
        );
        assert_eq!(facts[0].user_id, "alice");
    }
}
