//! Incremental stream parser — real-time detection of tool_use blocks in streaming mode.
//!
//! This replaces the old `run_streaming()` approach that collected the
//! full stream before processing. The incremental parser detects tool
//! intent as soon as the tool call's name field appears in the stream,
//! allowing the UI to show the agent's intent before arguments are
//! fully generated.
//!
//! This addresses Issue #18: streaming mode currently collects complete
//! stream before processing, so users must wait for the entire stream
//! to finish before knowing if a tool call is happening.
//!
//! Inspired by Claude Code's approach: incremental parsing of tool_use
//! blocks during streaming, so users can see the agent deciding to
//! call a tool even before the arguments are fully generated.

use std::collections::HashMap;
use std::sync::Arc;

use oneai_core::{ContentBlock, InferenceStreamChunk};

// ─── StreamEvent ────────────────────────────────────────────────────────────

/// Events emitted during incremental stream parsing.
///
/// These events allow the UI to react to stream content as it arrives,
/// rather than waiting for the entire stream to complete.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Text content received (partial — accumulates over multiple chunks).
    TextFragment {
        /// The text fragment received.
        text: String,
    },

    /// Thinking/reasoning content fragment received (partial — accumulates).
    /// Extended thinking models (Anthropic, DeepSeek) produce Thinking blocks
    /// during streaming. Each chunk may contain a partial thinking fragment.
    ThinkingFragment {
        /// The thinking text fragment received.
        text: String,
    },

    /// A tool call intent has been detected.
    /// The tool name is known, but arguments may still be streaming.
    ToolIntentDetected {
        /// The tool call ID.
        call_id: String,
        /// The tool name that will be called.
        tool_name: String,
    },

    /// A tool call's arguments have been fully received.
    ToolCallComplete {
        /// The tool call ID.
        call_id: String,
        /// The tool name.
        tool_name: String,
        /// The complete arguments (JSON string).
        args: String,
    },

    /// The stream has finished (is_final chunk received).
    StreamComplete {
        /// All assembled content blocks.
        assembled_content: Vec<ContentBlock>,
    },
}

// ─── ToolCallBuilder ────────────────────────────────────────────────────────

/// Builder for incrementally assembling a tool call from streaming chunks.
///
/// Tool calls arrive in multiple chunks:
/// 1. First chunk: contains the tool call ID and name
/// 2. Subsequent chunks: contain argument fragments
/// 3. Final chunk: signals completion (or next tool call begins)
struct ToolCallBuilder {
    /// The tool call ID.
    id: String,
    /// The tool name.
    name: String,
    /// Accumulated argument string.
    args_buffer: String,
    /// Whether the name has been fully received.
    #[allow(dead_code)]
    name_complete: bool,
}

// ─── IncrementalStreamParser ────────────────────────────────────────────────

/// Incremental stream parser — processes streaming chunks and emits
/// StreamEvents for real-time UI updates.
///
/// Key improvement over the old approach:
/// - When a tool call's name field appears in a chunk, the parser
///   immediately emits `ToolIntentDetected`, allowing the UI to show
///   "Agent is about to call tool X" before the arguments stream in.
/// - Arguments are accumulated incrementally and emitted as
///   `ToolCallComplete` when fully received.
/// - Text content is accumulated in a buffer and emitted as
///   `TextFragment` for real-time display.
///
/// Usage in AgentLoop:
/// ```ignore
/// let mut stream = self.provider.infer_stream(request).await?;
/// while let Some(chunk) = stream.next().await {
///     if let Some(event) = self.stream_parser.process_chunk(chunk) {
///         on_event(event); // UI callback
///     }
/// }
/// let assembled = self.stream_parser.finalize();
/// ```
pub struct IncrementalStreamParser {
    /// Buffer for accumulating text content.
    text_buffer: String,

    /// Buffer for accumulating thinking/reasoning content.
    /// Extended thinking models produce ContentBlock::Thinking during streaming.
    /// These fragments are accumulated and emitted as ThinkingFragment events,
    /// then finalized as a ContentBlock::Thinking block.
    thinking_buffer: String,

    /// Builders for tool calls being assembled.
    /// Keyed by tool call ID.
    tool_call_builders: HashMap<String, ToolCallBuilder>,

    /// The current tool call being assembled (if streaming args).
    current_tool_call_id: Option<String>,

    /// Callback for stream events (typically UI notification).
    on_event: Option<Arc<dyn Fn(StreamEvent) + Send + Sync>>,
}

impl IncrementalStreamParser {
    /// Create a new incremental stream parser.
    pub fn new() -> Self {
        Self {
            text_buffer: String::new(),
            thinking_buffer: String::new(),
            tool_call_builders: HashMap::new(),
            current_tool_call_id: None,
            on_event: None,
        }
    }

    /// Create a parser with an event callback.
    pub fn with_event_callback(on_event: Arc<dyn Fn(StreamEvent) + Send + Sync>) -> Self {
        Self {
            text_buffer: String::new(),
            thinking_buffer: String::new(),
            tool_call_builders: HashMap::new(),
            current_tool_call_id: None,
            on_event: Some(on_event),
        }
    }

    /// Process a streaming chunk and emit any StreamEvents.
    ///
    /// Returns the events that were generated from this chunk.
    /// Key behavior:
    /// - Text fragments → emit TextFragment immediately
    /// - Tool call ID + name → emit ToolIntentDetected immediately
    /// - Tool call arg continuation → accumulate silently
    /// - End of tool call → emit ToolCallComplete
    pub fn process_chunk(&mut self, chunk: InferenceStreamChunk) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        for block in &chunk.content {
            match block {
                ContentBlock::Text { text } => {
                    self.text_buffer.push_str(text);
                    let event = StreamEvent::TextFragment { text: text.clone() };
                    events.push(event.clone());
                    self.emit_event(event);
                }
                ContentBlock::ToolCall { id, name, args } => {
                    // ─── Routing contract ────────────────────────────────────
                    // OpenAI streams parallel tool calls distinguished by `index`,
                    // with argument fragments that interleave across calls. The
                    // provider (openai.rs) translates `index → id` and echoes the
                    // full `id` on every fragment, so we route by `id` here — NOT
                    // by a single "current" pointer (which misroutes interleaved
                    // fragments) and NOT by `id.is_empty()` (which breaks providers
                    // that echo `id` on every chunk).
                    //
                    //   • name non-empty            → new tool call (intent)
                    //   • name empty, args non-empty → arg fragment; route by id
                    //     (fall back to current only if id is absent)
                    //   • id only, no name/args      → no payload; ignore
                    if !name.is_empty() {
                        // New tool call. Parallel calls coexist — do NOT finalize
                        // the previous builder here; all pending builders are
                        // completed together at is_final. If a builder with this
                        // id already exists (e.g. Anthropic content_block_stop
                        // re-emits id+name+full-args), overwrite it with the
                        // now-complete args.
                        let builder = ToolCallBuilder {
                            id: id.clone(),
                            name: name.clone(),
                            args_buffer: if !args.is_empty() { args.clone() } else { String::new() },
                            name_complete: true,
                        };
                        let is_new = !self.tool_call_builders.contains_key(id);
                        self.tool_call_builders.insert(id.clone(), builder);
                        self.current_tool_call_id = Some(id.clone());

                        // Emit intent detection only for genuinely new calls,
                        // not for re-emissions of an already-known id.
                        if is_new {
                            let event = StreamEvent::ToolIntentDetected {
                                call_id: id.clone(),
                                tool_name: name.clone(),
                            };
                            events.push(event.clone());
                            self.emit_event(event);
                        }
                    } else if !args.is_empty() {
                        // Argument continuation fragment.
                        let target_id = if !id.is_empty() {
                            Some(id.clone())
                        } else {
                            self.current_tool_call_id.clone()
                        };
                        match target_id {
                            Some(tid) => {
                                if let Some(builder) = self.tool_call_builders.get_mut(&tid) {
                                    builder.args_buffer.push_str(args);
                                } else {
                                    // No builder for this id — the model streamed an
                                    // arg fragment without a preceding intent chunk.
                                    // Log it (observability parity with openai.rs's
                                    // JSON-parse warn) rather than silently dropping.
                                    tracing::warn!(
                                        "tool-call arg fragment with no matching builder \
                                         (id={:?}, current={:?}); fragment dropped",
                                        id, self.current_tool_call_id
                                    );
                                }
                            }
                            None => {
                                tracing::warn!(
                                    "tool-call arg fragment with no routable id \
                                     (no id on chunk and no current tool call); \
                                     fragment dropped"
                                );
                            }
                        }
                    } else if !id.is_empty() {
                        // id-only echo with no name/args payload — nothing
                        // actionable. Debug-log for completeness.
                        tracing::debug!(
                            "tool-call chunk with id only, no name/args (ignored): id={:?}",
                            id
                        );
                    }
                }
                ContentBlock::Thinking { text } => {
                    // Extended thinking/reasoning content — accumulate into buffer
                    // and emit ThinkingFragment events for real-time UI display.
                    // This addresses the known gap where Thinking blocks were
                    // silently dropped by `_ => {}` in process_chunk().
                    self.thinking_buffer.push_str(text);
                    let event = StreamEvent::ThinkingFragment { text: text.clone() };
                    events.push(event.clone());
                    self.emit_event(event);
                }
                _ => {} // Image, File, ToolResult — not handled in streaming
            }
        }

        // If this is the final chunk, emit completion events.
        // CRITICAL: Do NOT call finalize() here — finalize() clears the
        // text/thinking/tool buffers, which would cause the second finalize()
        // call (from run_streaming_iteration_async) to return empty content.
        // Instead, emit completion events without clearing buffers.
        // The outer code calls finalize() once after the stream loop ends.
        if chunk.is_final {
            events.extend(self.emit_completion_events());
        }

        events
    }

    /// Finalize the stream — complete any pending tool calls and
    /// return all assembled content blocks.
    pub fn finalize(&mut self) -> Vec<ContentBlock> {
        let mut content_blocks: Vec<ContentBlock> = Vec::new();

        // Add thinking content (before text — thinking precedes the answer)
        if !self.thinking_buffer.is_empty() {
            content_blocks.push(ContentBlock::Thinking {
                text: self.thinking_buffer.clone(),
            });
        }

        // Add text content
        if !self.text_buffer.is_empty() {
            content_blocks.push(ContentBlock::Text {
                text: self.text_buffer.clone(),
            });
        }

        // Add all completed tool calls
        for (_, builder) in &self.tool_call_builders {
            content_blocks.push(ContentBlock::ToolCall {
                id: builder.id.clone(),
                name: builder.name.clone(),
                args: builder.args_buffer.clone(),
            });
        }

        // Clear buffers
        self.text_buffer.clear();
        self.thinking_buffer.clear();
        self.tool_call_builders.clear();
        self.current_tool_call_id = None;

        content_blocks
    }

    /// Finalize stream events for the end of streaming.
    /// This clears buffers and returns assembled content.
    /// Used for explicit finalize() calls (not during stream processing).
    #[allow(dead_code)]
    fn finalize_stream(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        // Complete any pending tool call
        if let Some(current_id) = self.current_tool_call_id.take() {
            if let Some(builder) = self.tool_call_builders.get(&current_id) {
                let event = StreamEvent::ToolCallComplete {
                    call_id: builder.id.clone(),
                    tool_name: builder.name.clone(),
                    args: builder.args_buffer.clone(),
                };
                events.push(event.clone());
                self.emit_event(event);
            }
        }

        // Emit assembled content
        let assembled = self.finalize();
        let event = StreamEvent::StreamComplete {
            assembled_content: assembled,
        };
        events.push(event.clone());
        self.emit_event(event);

        events
    }

    /// Emit completion events for the end of streaming WITHOUT clearing buffers.
    ///
    /// This is used in process_chunk() when is_final=true. It emits
    /// `ToolCallComplete` for **every** pending tool-call builder (parallel
    /// calls coexist — we must complete all of them, not just the last one
    /// pointed at by `current_tool_call_id`) and a `StreamComplete` event,
    /// but does NOT call finalize() (which would clear the text/thinking/tool
    /// buffers). The actual buffer clearing happens in the finalize() call
    /// from run_streaming_iteration_async, ensuring buffers are only cleared
    /// once.
    fn emit_completion_events(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        // Complete every pending tool call (parallel calls coexist).
        // Iterate over a snapshot of the ids because emit_event may take a
        // &self callback while we hold &mut self — collect first, then look up.
        self.current_tool_call_id = None;
        let ids: Vec<String> = self.tool_call_builders.keys().cloned().collect();
        for tid in ids {
            if let Some(builder) = self.tool_call_builders.get(&tid) {
                let event = StreamEvent::ToolCallComplete {
                    call_id: builder.id.clone(),
                    tool_name: builder.name.clone(),
                    args: builder.args_buffer.clone(),
                };
                events.push(event.clone());
                self.emit_event(event);
            }
        }

        // Emit StreamComplete with a preview of assembled content
        // (without calling finalize, which would clear buffers)
        let preview_blocks: Vec<ContentBlock> = if !self.thinking_buffer.is_empty() {
            vec![ContentBlock::Thinking { text: self.thinking_buffer.clone() }]
        } else if !self.text_buffer.is_empty() {
            vec![ContentBlock::Text { text: self.text_buffer.clone() }]
        } else {
            self.tool_call_builders.values().map(|b| ContentBlock::ToolCall {
                id: b.id.clone(),
                name: b.name.clone(),
                args: b.args_buffer.clone(),
            }).collect()
        };
        let event = StreamEvent::StreamComplete {
            assembled_content: preview_blocks,
        };
        events.push(event.clone());
        self.emit_event(event);

        events
    }

    /// Emit a stream event to the callback (if configured).
    fn emit_event(&self, event: StreamEvent) {
        if let Some(callback) = &self.on_event {
            callback(event);
        }
    }
}

impl Default for IncrementalStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::{ContentBlock, InferenceStreamChunk};

    fn tc(id: &str, name: &str, args: &str) -> ContentBlock {
        ContentBlock::ToolCall { id: id.to_string(), name: name.to_string(), args: args.to_string() }
    }

    fn chunk(blocks: Vec<ContentBlock>, is_final: bool) -> InferenceStreamChunk {
        InferenceStreamChunk { content: blocks, is_final, usage: None, model: None }
    }

    /// Collect the tool calls assembled by a sequence of chunks.
    fn assemble(chunks: Vec<InferenceStreamChunk>) -> Vec<(String, String, String)> {
        let mut parser = IncrementalStreamParser::new();
        let mut events_all = Vec::new();
        for c in chunks {
            events_all.extend(parser.process_chunk(c));
        }
        // finalize() returns the assembled content blocks
        let blocks = parser.finalize();
        blocks.into_iter().filter_map(|b| match b {
            ContentBlock::ToolCall { id, name, args } => Some((id, name, args)),
            _ => None,
        }).collect()
    }

    /// Collect ToolCallComplete events (id, name, args) in order.
    fn complete_events(chunks: Vec<InferenceStreamChunk>) -> Vec<(String, String, String)> {
        let mut parser = IncrementalStreamParser::new();
        let mut out = Vec::new();
        for c in chunks {
            for e in parser.process_chunk(c) {
                if let StreamEvent::ToolCallComplete { call_id, tool_name, args } = e {
                    out.push((call_id, tool_name, args));
                }
            }
        }
        out
    }

    /// Regression for bug (a): interleaved parallel tool calls must NOT
    /// concatenate args. The provider echoes the full `id` on every fragment
    /// (the new openai.rs contract), so the parser routes by id.
    #[test]
    fn parallel_interleaved_tool_calls_route_by_id() {
        let chunks = vec![
            chunk(vec![tc("A", "read_file", "")], false),
            chunk(vec![tc("B", "grep", "")], false),
            chunk(vec![tc("A", "", "{\"pat")], false),
            chunk(vec![tc("B", "", "\"key\"")], false),
            chunk(vec![tc("A", "", "h\":\"x\"}")], false),
            chunk(vec![], true),
        ];
        let assembled = assemble(chunks);
        assert_eq!(assembled.len(), 2);
        let a = assembled.iter().find(|(id, _, _)| id == "A").unwrap();
        let b = assembled.iter().find(|(id, _, _)| id == "B").unwrap();
        assert_eq!(a.1, "read_file");
        assert_eq!(a.2, "{\"path\":\"x\"}");
        assert_eq!(b.1, "grep");
        assert_eq!(b.2, "\"key\"");
    }

    /// Both parallel tool calls must fire a ToolCallComplete event at is_final
    /// (previously only the last `current` was completed).
    #[test]
    fn parallel_tool_calls_both_complete_events_fire() {
        let chunks = vec![
            chunk(vec![tc("A", "read_file", "")], false),
            chunk(vec![tc("B", "grep", "")], false),
            chunk(vec![tc("A", "", "{}")], false),
            chunk(vec![tc("B", "", "{}")], false),
            chunk(vec![], true),
        ];
        let evts = complete_events(chunks);
        assert_eq!(evts.len(), 2, "both parallel calls should complete");
        assert!(evts.iter().any(|(id, _, _)| id == "A"));
        assert!(evts.iter().any(|(id, _, _)| id == "B"));
    }

    /// Regression for bug (b): a provider that echoes the full `id` on every
    /// continuation chunk (id non-empty, name empty, args fragment). The old
    /// parser dropped these silently; the new parser routes by id.
    #[test]
    fn provider_echoes_id_on_every_fragment() {
        let chunks = vec![
            chunk(vec![tc("call_1", "environment", "")], false),
            chunk(vec![tc("call_1", "", "{\"info")], false),
            chunk(vec![tc("call_1", "", "_type\":\"all\"}")], false),
            chunk(vec![], true),
        ];
        let assembled = assemble(chunks);
        assert_eq!(assembled.len(), 1);
        assert_eq!(assembled[0].0, "call_1");
        assert_eq!(assembled[0].1, "environment");
        assert_eq!(assembled[0].2, "{\"info_type\":\"all\"}");
    }

    /// The legacy OpenAI contract (id empty on fragments) must still work via
    /// the `current` fallback for providers that don't echo id.
    #[test]
    fn legacy_empty_id_fragments_route_via_current() {
        let chunks = vec![
            chunk(vec![tc("call_1", "environment", "")], false),
            chunk(vec![tc("", "", "{\"info")], false),
            chunk(vec![tc("", "", "_type\":\"all\"}")], false),
            chunk(vec![], true),
        ];
        let assembled = assemble(chunks);
        assert_eq!(assembled.len(), 1);
        assert_eq!(assembled[0].0, "call_1");
        assert_eq!(assembled[0].2, "{\"info_type\":\"all\"}");
    }

    /// Anthropic contract: id+name+full-args emitted once at content_block_stop,
    /// no arg fragments. Must still assemble correctly and not double-fire intent.
    #[test]
    fn anthropic_style_single_complete_chunk() {
        let chunks = vec![
            // content_block_start: id+name, empty args
            chunk(vec![tc("call_1", "environment", "")], false),
            // content_block_stop: id+name+full args (name re-emitted)
            chunk(vec![tc("call_1", "environment", "{\"info_type\":\"all\"}")], false),
            chunk(vec![], true),
        ];
        let assembled = assemble(chunks);
        assert_eq!(assembled.len(), 1);
        assert_eq!(assembled[0].2, "{\"info_type\":\"all\"}");

        // Intent should fire exactly once (the re-emission with the same id
        // is not a new call).
        let mut parser = IncrementalStreamParser::new();
        let mut intent = 0;
        for c in [
            chunk(vec![tc("call_1", "environment", "")], false),
            chunk(vec![tc("call_1", "environment", "{}")], false),
            chunk(vec![], true),
        ] {
            for e in parser.process_chunk(c) {
                if matches!(e, StreamEvent::ToolIntentDetected { .. }) { intent += 1; }
            }
        }
        assert_eq!(intent, 1, "intent should fire once per unique id");
    }

    /// First-chunk may carry an initial args fragment alongside the name.
    #[test]
    fn first_chunk_with_initial_args_fragment() {
        let chunks = vec![
            chunk(vec![tc("call_1", "environment", "{\"info")], false),
            chunk(vec![tc("call_1", "", "_type\":\"all\"}")], false),
            chunk(vec![], true),
        ];
        let assembled = assemble(chunks);
        assert_eq!(assembled[0].2, "{\"info_type\":\"all\"}");
    }
}