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
use oneai_core::error::Result;

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
                    // New tool call detected (has an ID)
                    if !id.is_empty() && !name.is_empty() {
                        // Finalize any previous tool call
                        if let Some(prev_id) = self.current_tool_call_id.take() {
                            if let Some(builder) = self.tool_call_builders.get_mut(&prev_id) {
                                let event = StreamEvent::ToolCallComplete {
                                    call_id: builder.id.clone(),
                                    tool_name: builder.name.clone(),
                                    args: builder.args_buffer.clone(),
                                };
                                events.push(event.clone());
                                self.emit_event(event);
                            }
                        }

                        // Start new tool call
                        let builder = ToolCallBuilder {
                            id: id.clone(),
                            name: name.clone(),
                            args_buffer: if !args.is_empty() { args.clone() } else { String::new() },
                            name_complete: true,
                        };
                        self.tool_call_builders.insert(id.clone(), builder);
                        self.current_tool_call_id = Some(id.clone());

                        // Emit intent detection immediately
                        let event = StreamEvent::ToolIntentDetected {
                            call_id: id.clone(),
                            tool_name: name.clone(),
                        };
                        events.push(event.clone());
                        self.emit_event(event);
                    }
                    // Argument continuation (empty id, non-empty args)
                    else if id.is_empty() && !args.is_empty() {
                        if let Some(current_id) = &self.current_tool_call_id {
                            if let Some(builder) = self.tool_call_builders.get_mut(current_id) {
                                builder.args_buffer.push_str(args);
                            }
                        }
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
    /// ToolCallComplete events for any pending tool calls and a
    /// StreamComplete event, but does NOT call finalize() (which would
    /// clear the text/thinking/tool buffers). The actual buffer clearing
    /// happens in the finalize() call from run_streaming_iteration_async,
    /// ensuring buffers are only cleared once.
    fn emit_completion_events(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        // Complete any pending tool call (without clearing tool_call_builders)
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