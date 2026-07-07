//! Token budget management — hierarchical context budget allocation
//! that replaces hardcoded max_iterations with a natural budget constraint.
//!
//! Key concepts:
//! - `TokenBudget`: The total token budget for a session/agent
//! - `BudgetAllocation`: Proportional allocation to different context sources
//! - `ContextBudgetManager`: Orchestrates budget checking and auto-compression
//!
//! Note: The ContextBudgetManager's compressor dependency is defined here
//! as a trait interface. The actual ContextCompressor lives in oneai-memory,
//! and is injected at runtime through the AppBuilder. This keeps oneai-core
//! independent of oneai-memory.

use std::sync::Arc;

use crate::Conversation;
use crate::InferenceResponse;
use crate::error::Result;
use crate::ContentBlock;
use crate::Message;
use crate::Role;
use crate::traits::DiscardedSink;

// ─── ContextCompressor trait (defined in core for dependency inversion) ────

/// Context compressor trait — defined here so that ContextBudgetManager
/// can accept any compressor implementation without depending on oneai-memory.
///
/// The actual implementation lives in oneai-memory::ContextCompressor.
#[async_trait::async_trait]
pub trait ContextCompressorTrait: Send + Sync {
    /// Estimate the token count of a conversation.
    fn estimate_tokens(&self, conversation: &Conversation) -> usize;

    /// Estimate the token count of a single message.
    fn estimate_tokens_of_message(&self, msg: &Message) -> usize;

    /// Compress a conversation when threshold is exceeded.
    async fn compress(&self, conversation: &Conversation) -> Result<CompressedResult>;
}

/// Result of context compression.
#[derive(Debug, Clone)]
pub struct CompressedResult {
    /// The compressed conversation.
    pub compressed_conversation: Conversation,
    /// The generated summary (if compression was performed).
    pub summary: Option<String>,
    /// The original messages that were summarized/truncated away during
    /// compression.
    ///
    /// Carrying these (rather than dropping them on the trait boundary) is the
    /// seam for the "压缩即丢失" closure: the compression-coupled
    /// `FactExtractor` runs over these to extract durable facts into the
    /// archival tier, so information compressed out of context is not lost.
    /// Empty when no compression occurred.
    pub discarded_messages: Vec<Message>,
}

/// No-op compressor — does nothing, returns the conversation unchanged.
/// Used as a default when no real compressor is available (e.g., in CLI demo mode).
///
/// **Warning**: NoopCompressor does NOT actually compress. Long conversations
/// will overflow the model's context window. Use `TruncationCompressor` as
/// the default fallback instead — it truncates without requiring an LLM.
pub struct NoopCompressor;

#[async_trait::async_trait]
impl ContextCompressorTrait for NoopCompressor {
    fn estimate_tokens(&self, conversation: &Conversation) -> usize {
        // Rough estimate: ~4 chars per token
        conversation.messages.iter()
            .map(|m| m.content.iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.len(),
                    _ => 50, // rough estimate for non-text blocks
                })
                .sum::<usize>())
            .sum::<usize>() / 4
    }

    fn estimate_tokens_of_message(&self, msg: &Message) -> usize {
        msg.content.iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.len(),
                _ => 50,
            })
            .sum::<usize>() / 4
    }

    async fn compress(&self, conversation: &Conversation) -> Result<CompressedResult> {
        Ok(CompressedResult {
            compressed_conversation: conversation.clone(),
            summary: None,
            discarded_messages: Vec::new(),
        })
    }
}

// ─── TruncationCompressor ────────────────────────────────────────────────────

/// Truncation-based compressor — always works without requiring an LLM.
///
/// This is the recommended default compressor. Unlike `NoopCompressor` which
/// does nothing, TruncationCompressor actively prevents context overflow by:
///
/// 1. Keeping system messages intact (they're essential for agent behavior)
/// 2. Keeping the most recent N turns intact (they're the most relevant)
/// 3. Truncating older turns to their first `max_summary_chars` characters
/// 4. Truncating tool results to `max_tool_result_chars` characters
/// 5. Adding a summary message indicating what was truncated
///
/// This approach trades information completeness for guaranteed context safety.
/// It's the right default because:
/// - It never requires an LLM call (no dependency, no cost, no latency)
/// - It always produces a valid conversation (never overflows the window)
/// - It preserves the most important context (system + recent turns)
///
/// For higher-quality compression, use `ContextCompressor` from oneai-memory
/// which uses an LLM to summarize — but TruncationCompressor is the safe fallback.
pub struct TruncationCompressor {
    /// Maximum length for tool result content (in characters).
    /// Long shell outputs, file contents, etc. are truncated to this length.
    pub max_tool_result_chars: usize,

    /// Maximum length for older turn summaries (in characters).
    /// Each older message is truncated to this length with a "[...truncated]" suffix.
    pub max_summary_chars: usize,

    /// Number of recent turns to keep intact (not truncated).
    /// Recent turns are the most relevant for the agent's current reasoning.
    pub keep_recent_turns: usize,
}

impl TruncationCompressor {
    /// Create a new TruncationCompressor with default settings.
    pub fn new() -> Self {
        Self {
            max_tool_result_chars: 2000,
            max_summary_chars: 200,
            keep_recent_turns: 6, // Keep last 3 exchanges (user+assistant pairs)
        }
    }

    /// Create with custom settings.
    pub fn with_config(
        max_tool_result_chars: usize,
        max_summary_chars: usize,
        keep_recent_turns: usize,
    ) -> Self {
        Self {
            max_tool_result_chars,
            max_summary_chars,
            keep_recent_turns,
        }
    }
}

impl Default for TruncationCompressor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ContextCompressorTrait for TruncationCompressor {
    fn estimate_tokens(&self, conversation: &Conversation) -> usize {
        // Same heuristic as NoopCompressor: ~4 chars per token
        conversation.messages.iter()
            .map(|m| m.content.iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.len(),
                    _ => 50,
                })
                .sum::<usize>())
            .sum::<usize>() / 4
    }

    fn estimate_tokens_of_message(&self, msg: &Message) -> usize {
        msg.content.iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.len(),
                _ => 50,
            })
            .sum::<usize>() / 4
    }

    async fn compress(&self, conversation: &Conversation) -> Result<CompressedResult> {
        let total_messages = conversation.messages.len();

        // If conversation is short enough, no compression needed
        if total_messages <= self.keep_recent_turns + 1 { // +1 for system message
            return Ok(CompressedResult {
                compressed_conversation: conversation.clone(),
                summary: None,
                discarded_messages: Vec::new(),
            });
        }

        let mut compressed = Conversation::with_id(conversation.id.clone());
        compressed.metadata = conversation.metadata.clone();

        // Collect info about what was truncated for the summary
        let mut truncated_count = 0;
        // Original older (non-system, non-recent) messages — surfaced for
        // fact extraction so compressed-away content isn't lost.
        let mut discarded: Vec<Message> = Vec::new();

        // The first user message is the original task — pin it verbatim (Q2)
        // rather than truncating it to a 200-char stub. Identified once, then
        // treated like system/recent for keep-intact purposes.
        let first_user_idx = conversation.messages.iter()
            .position(|m| m.role == Role::User);

        // Process messages in order
        for (idx, msg) in conversation.messages.iter().enumerate() {
            // Determine if this is a "recent" message (keep intact) or "older" (truncate)
            let is_recent = idx >= total_messages - self.keep_recent_turns;
            let is_system = msg.role == Role::System;
            let is_pinned_first_user = first_user_idx == Some(idx);

            if is_system || is_recent || is_pinned_first_user {
                // Keep system messages and recent turns intact
                // But truncate long tool results even in recent turns
                let processed_content = msg.content.iter().map(|block| {
                    match block {
                        ContentBlock::ToolResult { call_id, content } => {
                            if content.len() > self.max_tool_result_chars {
                                ContentBlock::ToolResult {
                                    call_id: call_id.clone(),
                                    content: format!("{}{}",
                                        &content[..self.max_tool_result_chars.min(content.len())],
                                        "\n[...output truncated]"
                                    ),
                                }
                            } else {
                                block.clone()
                            }
                        }
                        ContentBlock::Text { text } => {
                            // Truncate very long text in recent turns (e.g., large file reads)
                            if !is_system && text.len() > self.max_tool_result_chars {
                                ContentBlock::Text {
                                    text: format!("{}{}",
                                        &text[..self.max_tool_result_chars.min(text.len())],
                                        "\n[...content truncated]"
                                    ),
                                }
                            } else {
                                block.clone()
                            }
                        }
                        _ => block.clone(),
                    }
                }).collect::<Vec<_>>();

                compressed.add_message(Message {
                    role: msg.role,
                    content: processed_content,
                    metadata: msg.metadata.clone(),
                });
            } else {
                // Older message — truncate to summary
                truncated_count += 1;
                discarded.push(msg.clone());
                let text = msg.text_content();
                if text.len() > self.max_summary_chars {
                    let summary = format!(
                        "[{}]: {} [...truncated]",
                        match msg.role {
                            Role::System => "System",
                            Role::User => "User",
                            Role::Assistant => "Assistant",
                            Role::Tool => "Tool",
                        },
                        &text[..self.max_summary_chars.min(text.len())]
                    );
                    compressed.add_message(Message::system(summary));
                } else if !text.is_empty() {
                    compressed.add_message(Message::system(format!(
                        "[{} (older)]: {}",
                        match msg.role {
                            Role::System => "System",
                            Role::User => "User",
                            Role::Assistant => "Assistant",
                            Role::Tool => "Tool",
                        },
                        text
                    )));
                }
                // Skip empty older messages entirely
            }
        }

        // Add truncation summary if any messages were truncated
        let summary = if truncated_count > 0 {
            Some(format!(
                "Compressed: {} older messages truncated to {}-char summaries. \
                {} recent turns preserved intact. Tool outputs capped at {} chars.",
                truncated_count, self.max_summary_chars,
                self.keep_recent_turns, self.max_tool_result_chars
            ))
        } else {
            None
        };

        Ok(CompressedResult {
            compressed_conversation: compressed,
            summary,
            discarded_messages: discarded,
        })
    }
}

// ─── TokenBudget ────────────────────────────────────────────────────────────

/// A token budget — the total number of tokens available for a session or sub-agent.
///
/// The budget is consumed by:
/// - Prompt tokens (input to the model)
/// - Completion tokens (output from the model)
/// - Tool result tokens (fed back into the conversation)
///
/// When `remaining()` drops below `min_iteration_cost`, the loop should terminate.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// Total token budget for this session.
    pub total: u32,

    /// Tokens consumed so far (prompt + completion + tool results).
    pub consumed: u32,
}

impl TokenBudget {
    /// Create a new budget with the given total.
    pub fn new(total: u32) -> Self {
        Self { total, consumed: 0 }
    }

    /// Create an unlimited budget (for testing or when no budget constraint is needed).
    pub fn unlimited() -> Self {
        Self { total: u32::MAX, consumed: 0 }
    }

    /// Create a budget based on a model's context window size.
    /// Uses 80% of the context window as the effective budget (leaving room for overhead).
    pub fn from_context_window(context_window_size: u32) -> Self {
        Self {
            total: (context_window_size as f32 * 0.8) as u32,
            consumed: 0,
        }
    }

    /// Get the remaining tokens.
    pub fn remaining(&self) -> u32 {
        self.total.saturating_sub(self.consumed)
    }

    /// Record token consumption from an inference response.
    pub fn record_usage(&mut self, prompt_tokens: u32, completion_tokens: u32) {
        self.consumed += prompt_tokens + completion_tokens;
    }

    /// Check if the budget can support one more iteration with the estimated cost.
    pub fn can_support_iteration(&self, estimated_cost: u32) -> bool {
        self.remaining() >= estimated_cost
    }

    /// Get the estimated maximum number of remaining iterations.
    pub fn estimated_remaining_iterations(&self, per_iteration_cost: u32) -> u32 {
        if per_iteration_cost == 0 { return u32::MAX; }
        self.remaining() / per_iteration_cost
    }
}

// ─── BudgetAllocation ───────────────────────────────────────────────────────

/// Proportional allocation of the context budget to different sources.
///
/// When the total estimated tokens exceed the budget, sources are trimmed
/// in order of priority (tool_results first, then older turns, then skills/retrieved).
#[derive(Debug, Clone)]
pub struct BudgetAllocation {
    /// Fraction of budget for system prompt (default: 10%).
    pub system_prompt: f32,

    /// Fraction of budget for recent conversation turns (default: 30%).
    pub recent_turns: f32,

    /// Fraction of budget for tool results (default: 25%).
    /// This is the largest allocation because tool outputs can be very long.
    pub tool_results: f32,

    /// Fraction of budget for skill descriptions (default: 10%).
    pub skills: f32,

    /// Fraction of budget for retrieved context (default: 15%).
    pub retrieved: f32,

    /// Fraction of budget reserved for overhead (default: 10%).
    pub overhead: f32,
}

impl Default for BudgetAllocation {
    fn default() -> Self {
        Self {
            system_prompt: 0.10,
            recent_turns: 0.30,
            tool_results: 0.25,
            skills: 0.10,
            retrieved: 0.15,
            overhead: 0.10,
        }
    }
}

impl BudgetAllocation {
    /// Validate that all fractions sum to approximately 1.0.
    pub fn validate(&self) -> bool {
        let sum = self.system_prompt + self.recent_turns + self.tool_results
            + self.skills + self.retrieved + self.overhead;
        (sum - 1.0).abs() < 0.01
    }

    /// Get the token budget for each source based on the total budget.
    pub fn allocate(&self, total_budget: u32) -> BudgetAllocationTokens {
        BudgetAllocationTokens {
            system_prompt: (total_budget as f32 * self.system_prompt) as u32,
            recent_turns: (total_budget as f32 * self.recent_turns) as u32,
            tool_results: (total_budget as f32 * self.tool_results) as u32,
            skills: (total_budget as f32 * self.skills) as u32,
            retrieved: (total_budget as f32 * self.retrieved) as u32,
            overhead: (total_budget as f32 * self.overhead) as u32,
        }
    }
}

/// Token-level allocation (computed from proportions and total budget).
#[derive(Debug, Clone)]
pub struct BudgetAllocationTokens {
    pub system_prompt: u32,
    pub recent_turns: u32,
    pub tool_results: u32,
    pub skills: u32,
    pub retrieved: u32,
    pub overhead: u32,
}

// ─── CompressionPriority ────────────────────────────────────────────────────

/// Priority order for trimming context sources when the budget is exceeded.
///
/// Sources are trimmed in this order:
/// 1. Tool results (usually the largest and least critical for future reasoning)
/// 2. Older conversation turns (can be compressed to a summary)
/// 3. Retrieved context (can be downgraded to keyword-only)
/// 4. Skills (can be reduced to name-only descriptions)
/// 5. Recent turns (last resort — these are the most important)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum CompressionPriority {
    /// Trim tool results first.
    ToolResults = 1,
    /// Trim older turns next.
    OlderTurns = 2,
    /// Trim retrieved context next.
    Retrieved = 3,
    /// Trim skill descriptions next.
    Skills = 4,
    /// Trim recent turns last (most important).
    RecentTurns = 5,
}

// ─── ContextBudgetManager ──────────────────────────────────────────────────

/// Context budget manager — orchestrates budget checking and auto-compression.
///
/// This is integrated into the AgentLoop's context assembly step.
/// Instead of manual `compress()` calls, the budget manager automatically
/// detects when the budget is exceeded and triggers compression.
///
/// Usage in AgentLoop:
/// ```ignore
/// if self.context_budget.needs_compression(&conversation) {
///     state.conversation = self.context_budget.compress(conversation)?;
/// }
/// ```
pub struct ContextBudgetManager {
    /// The total token budget for this session.
    budget: TokenBudget,

    /// The proportional allocation to different context sources.
    allocation: BudgetAllocation,

    /// The context compressor for summarizing older turns.
    /// Uses the trait interface defined above for dependency inversion.
    compressor: Arc<dyn ContextCompressorTrait>,

    /// Optional token counter — for accurate token estimation.
    /// When set, replaces the compressor's heuristic (~4 chars/token)
    /// with model-aware, language-aware token counting.
    /// Falls back to compressor heuristic when not set (backward compat).
    token_counter: Option<Arc<dyn crate::token_counter::TokenCounter>>,

    /// The model name used for token counting (needed by TokenCounter).
    model_name: Option<String>,

    /// Sink for messages discarded during compression (the "压缩即不丢"
    /// closure). When set, `compress` hands the discarded `Message`s to this
    /// sink before returning the compressed conversation, so raw transcript
    /// can be archived (conversation snapshot) for resume / audit / on-demand
    /// recall instead of being dropped. Fact extraction runs inside the
    /// compressor; this is the complementary raw-transcript archive.
    discarded_sink: Option<Arc<dyn DiscardedSink>>,

    /// Session id used to scope discarded snapshots. Set by the caller that
    /// wires the sink (the session knows its id).
    session_id: Option<String>,
}

impl ContextBudgetManager {
    /// Create a new budget manager.
    pub fn new(
        budget: TokenBudget,
        allocation: BudgetAllocation,
        compressor: Arc<dyn ContextCompressorTrait>,
    ) -> Self {
        assert!(allocation.validate(), "BudgetAllocation fractions must sum to ~1.0");
        Self { budget, allocation, compressor, token_counter: None, model_name: None, discarded_sink: None, session_id: None }
    }

    /// Create with default allocation based on a model's context window.
    pub fn from_context_window(
        context_window_size: u32,
        compressor: Arc<dyn ContextCompressorTrait>,
    ) -> Self {
        Self::new(
            TokenBudget::from_context_window(context_window_size),
            BudgetAllocation::default(),
            compressor,
        )
    }

    /// Set a token counter for accurate token estimation.
    ///
    /// When set, `needs_compression()` and `estimate_source_tokens()` use
    /// the TokenCounter instead of the compressor's heuristic (~4 chars/token).
    /// This produces more accurate budget checks, especially for CJK text.
    ///
    /// The `model_name` is required so the TokenCounter knows which
    /// tokenizer family to use for estimation.
    pub fn with_token_counter(
        mut self,
        tc: Arc<dyn crate::token_counter::TokenCounter>,
        model_name: String,
    ) -> Self {
        self.token_counter = Some(tc);
        self.model_name = Some(model_name);
        self
    }

    /// Attach a `DiscardedSink` so messages compressed out of the live
    /// conversation are archived (raw transcript) instead of dropped.
    /// The `session_id` scopes the archived snapshots.
    pub fn with_discarded_sink(
        mut self,
        sink: Arc<dyn DiscardedSink>,
        session_id: impl Into<String>,
    ) -> Self {
        self.discarded_sink = Some(sink);
        self.session_id = Some(session_id.into());
        self
    }

    /// Check if a conversation needs compression (total tokens exceed budget).
    pub fn needs_compression(&self, conversation: &Conversation) -> bool {
        let estimated_tokens = if let (Some(tc), Some(model)) = (&self.token_counter, &self.model_name) {
            // Use TokenCounter for accurate estimation
            tc.count_conversation_tokens(conversation, model) as usize
        } else {
            // Fallback to compressor heuristic
            self.compressor.estimate_tokens(conversation)
        };
        estimated_tokens > self.budget.total as usize
    }

    /// Compress a conversation when the budget is exceeded.
    ///
    /// Applies compression in priority order:
    /// 1. Trims tool results that exceed the tool_results budget
    /// 2. Compresses older conversation turns into a summary
    /// 3. Reduces retrieved context to keyword-only if needed
    ///
    /// Messages discarded (summarized away) by the compressor are handed to
    /// the optional `DiscardedSink` before returning, so raw transcript is
    /// archived rather than lost (the "压缩即不丢" closure).
    pub async fn compress(&self, conversation: Conversation) -> Result<Conversation> {
        // Step 1: Estimate token usage per source — drives whether Step 2 fires.
        let estimate = self.estimate_source_tokens(&conversation);
        let allocated = self.allocation.allocate(self.budget.total);

        // Step 2: If tool results exceed their allocation, truncate long
        // tool_result ContentBlocks in-place before summarization. This is the
        //无损截断 tier — the model keeps a capped view, and the pointer tells
        // it to reach for `memory_search` for the full output.
        let conversation = if estimate.tool_results as u32 > allocated.tool_results {
            truncate_tool_results(conversation, allocated.tool_results)
        } else {
            conversation
        };

        // Step 3: Compress older turns into a summary (compressor-specific).
        let result = self.compressor.compress(&conversation).await?;

        // Archive discarded raw transcript before it leaves the live context.
        if let Some(sink) = &self.discarded_sink {
            if !result.discarded_messages.is_empty() {
                let session_id = self.session_id.as_deref().unwrap_or("");
                if let Err(e) = sink.archive_discarded(session_id, result.discarded_messages.clone()).await {
                    tracing::warn!("discarded-sink archive failed (compression proceeds): {}", e);
                }
            }
        }

        Ok(result.compressed_conversation)
    }

    /// Get the current token budget.
    pub fn budget(&self) -> &TokenBudget {
        &self.budget
    }

    /// Record token usage from an inference response.
    pub fn record_usage(&mut self, response: &InferenceResponse) {
        self.budget.record_usage(
            response.usage.prompt_tokens,
            response.usage.completion_tokens,
        );
    }

    /// Check if the budget can support one more iteration.
    pub fn can_continue(&self, estimated_iteration_cost: u32) -> bool {
        self.budget.can_support_iteration(estimated_iteration_cost)
    }

    /// Estimate token usage per context source.
    fn estimate_source_tokens(&self, conversation: &Conversation) -> BudgetSourceEstimate {
        let mut system_prompt_tokens = 0;
        let mut recent_turns_tokens = 0;
        let mut tool_results_tokens = 0;

        for msg in &conversation.messages {
            let msg_tokens = self.compressor.estimate_tokens_of_message(msg);
            match msg.role {
                Role::System => system_prompt_tokens += msg_tokens,
                Role::Tool => tool_results_tokens += msg_tokens,
                _ => recent_turns_tokens += msg_tokens,
            }
        }

        BudgetSourceEstimate {
            system_prompt: system_prompt_tokens,
            recent_turns: recent_turns_tokens,
            tool_results: tool_results_tokens,
        }
    }
}

/// Estimated token usage per context source.
#[derive(Debug, Clone)]
pub struct BudgetSourceEstimate {
    pub system_prompt: usize,
    pub recent_turns: usize,
    pub tool_results: usize,
}

// ─── tool_result truncation (B4 Step 2) ─────────────────────────────────────

/// In-place无损 truncation of oversized `ToolResult` blocks.
///
/// When the estimated tool-result tokens exceed the `tool_results` budget
/// allocation, each `ContentBlock::ToolResult` is capped to a per-block char
/// limit (the budget converted to chars, distributed across blocks) and
/// suffixed with a pointer telling the model to recover the full output via
/// `memory_search` rather than carrying it in context. Other blocks are left
/// untouched. This is the "无损截断" tier that runs *before* summarization, so
/// long shell/file outputs stop being summarized away wholesale.
pub(crate) fn truncate_tool_results(conversation: Conversation, tool_results_token_budget: u32) -> Conversation {
    // Roughly 4 chars per token; cap each block at the full budget in chars
    // (a single runaway output shouldn't eat the whole window silently, but
    // per-block proportional capping is overkill here — the summary step still
    // bounds older turns).
    let max_chars = (tool_results_token_budget as usize).saturating_mul(4).max(1);

    let mut out = Conversation::with_id(conversation.id.clone());
    out.metadata = conversation.metadata.clone();
    for msg in conversation.messages {
        let truncated_content: Vec<ContentBlock> = msg.content.into_iter().map(|block| {
            match block {
                ContentBlock::ToolResult { call_id, content } => {
                    if content.chars().count() > max_chars {
                        let cut: String = content.chars().take(max_chars).collect();
                        ContentBlock::ToolResult {
                            call_id,
                            content: format!("{}\n[...output truncated — use memory_search for the full output]", cut),
                        }
                    } else {
                        ContentBlock::ToolResult { call_id, content }
                    }
                }
                other => other,
            }
        }).collect();
        out.add_message(Message {
            role: msg.role,
            content: truncated_content,
            metadata: msg.metadata,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_tool_results_caps_oversized_output_with_pointer() {
        // B4 Step 2: a runaway tool_result is capped and gets a memory_search
        // pointer rather than being summarized away wholesale.
        let long_output = "x".repeat(50_000);
        let mut conv = Conversation::with_id("c1".into());
        conv.add_message(Message::user("run it"));
        conv.add_message(Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                call_id: "call_1".to_string(),
                content: long_output,
            }],
            metadata: std::collections::HashMap::new(),
        });

        // 100-token tool_results budget → ~400 chars cap.
        let out = truncate_tool_results(conv, 100);
        let tool_msg = out.messages.iter().find(|m| m.role == Role::Tool).unwrap();
        let block = tool_msg.content.iter().next().unwrap();
        match block {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.contains("memory_search"));
                // Capped well below the original 50_000.
                assert!(content.chars().count() < 1000);
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn truncate_tool_results_leaves_short_outputs_intact() {
        let mut conv = Conversation::with_id("c1".into());
        conv.add_message(Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                call_id: "call_1".to_string(),
                content: "small output".to_string(),
            }],
            metadata: std::collections::HashMap::new(),
        });
        let out = truncate_tool_results(conv, 100);
        let block = out.messages[0].content.iter().next().unwrap();
        match block {
            ContentBlock::ToolResult { content, .. } => assert_eq!(content, "small output"),
            _ => panic!("expected ToolResult"),
        }
    }

    #[tokio::test]
    async fn truncation_compressor_pins_first_user_message() {
        // Q2: the original task (first user message) is kept verbatim even when
        // it would otherwise fall in the "older" segment and be truncated to a
        // 200-char stub. A long first user message survives intact.
        let compressor = TruncationCompressor::with_config(2000, 200, 2);
        let long_task = "Please refactor the entire authentication module to use JWT \
            tokens instead of session cookies, update all the middleware, and add tests.";
        let mut conv = Conversation::with_id("c1".into());
        conv.add_message(Message::user(long_task.to_string()));
        for i in 0..20 {
            conv.add_message(Message::assistant(format!("working {}", i)));
            conv.add_message(Message::user(format!("continue {}", i)));
        }
        let result = compressor.compress(&conv).await.unwrap();
        let text: String = result.compressed_conversation.messages.iter()
            .map(|m| m.text_content()).collect::<Vec<_>>().join("\n");
        assert!(text.contains(long_task),
            "first user message must be pinned verbatim by TruncationCompressor");
    }
}