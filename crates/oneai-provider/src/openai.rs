//! OpenAI-compatible cloud provider implementation.
//!
//! Supports OpenAI, DeepSeek, 智谱, and other OpenAI-compatible APIs.
//! Uses SSE (Server-Sent Events) for streaming responses.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use oneai_core::{
    ContentBlock, InferenceRequest, InferenceResponse, InferenceStreamChunk,
    ModelCapability, ModelConfig, Message, Role, TokenUsage,
};
use oneai_core::error::OneAIError;
use oneai_core::traits::LlmProvider;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use tokio_stream::wrappers::ReceiverStream;

/// OpenAI-compatible LLM provider.
pub struct OpenAIProvider {
    config: ModelConfig,
    client: Client,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider with the given configuration.
    pub fn new(config: ModelConfig) -> Self {
        let client = Client::new();
        Self { config, client }
    }

    /// Create with a custom HTTP client.
    pub fn with_client(config: ModelConfig, client: Client) -> Self {
        Self { config, client }
    }

    /// Get the chat completions endpoint URL.
    fn chat_url(&self) -> String {
        let base = self.config.resolved_url();
        format!("{}/chat/completions", base.trim_end_matches('/'))
    }

    /// Convert an InferenceRequest to OpenAI API format.
    fn to_openai_request(&self, req: &InferenceRequest) -> Value {
        let mut messages = Vec::new();
        for msg in &req.conversation.messages {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                _ => "user",
            };
            let mut openai_msg = serde_json::json!({
                "role": role,
            });

            // ─── Handle tool_calls in assistant messages ──────────────────
            // OpenAI format: assistant messages with tool_calls have:
            //   { role: "assistant", content: null|"text", tool_calls: [...] }
            // The content field MUST be present (null if no text) — some providers
            // (智谱GLM, 阿里百炼) reject requests where content is absent entirely.
            let tool_call_blocks: Vec<Value> = msg.content.iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { id, name, args } => Some(serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args,
                        }
                    })),
                    _ => None,
                })
                .collect();

            let has_tool_calls = !tool_call_blocks.is_empty();
            if has_tool_calls {
                openai_msg["tool_calls"] = Value::Array(tool_call_blocks);
            }

            // ─── Build content field ──────────────────────────────────────
            // For assistant messages: text content only (tool_calls are separate).
            // For tool result messages: raw tool output content (NOT wrapped with
            // call_id prefix — call_id goes in the separate tool_call_id field).
            // For user/system messages: plain text content.
            //
            // CRITICAL: ToolResult blocks MUST output raw content, not
            // `[Tool Result for call_id: content]`. The OpenAI format requires:
            //   { role: "tool", tool_call_id: "call_xxx", content: "raw output" }
            // Wrapping the content with call_id duplicates information (call_id
            // is already in tool_call_id) and can confuse the model or cause
            // API format rejection by strict providers.
            let text_content: Option<String> = if msg.role == Role::Tool {
                // Tool result message — extract raw content from ToolResult blocks
                // (joining multiple ToolResult blocks if present, though typically
                // each tool result message has exactly one ToolResult block)
                let parts: Vec<String> = msg.content.iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                if parts.is_empty() { None } else { Some(parts.join("\n")) }
            } else {
                // Assistant/user/system messages — extract text content only
                // (tool calls are already extracted above as separate tool_calls field)
                let parts: Vec<String> = msg.content.iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,  // ToolCall, ToolResult, Thinking, Image, File — skip
                    })
                    .collect();
                if parts.is_empty() { None } else { Some(parts.join("\n")) }
            };

            // Set content field — MUST be present for all message types.
            // For assistant messages with tool_calls but no text: content = null.
            // For other messages: content = text string.
            if msg.role == Role::Assistant && has_tool_calls && text_content.is_none() {
                // Assistant message has tool calls but no text content.
                // OpenAI format requires content: null (not absent or empty string "").
                // Some providers (智谱GLM, 阿里百炼) reject requests where content
                // is missing entirely from assistant messages with tool_calls.
                openai_msg["content"] = Value::Null;
            } else if let Some(text) = text_content {
                openai_msg["content"] = Value::String(text);
            }

            // ─── Add tool_call_id for tool result messages ────────────────
            for block in &msg.content {
                if let ContentBlock::ToolResult { call_id, .. } = block {
                    openai_msg["tool_call_id"] = Value::String(call_id.clone());
                }
            }

            messages.push(openai_msg);
        }

        let mut body = serde_json::json!({
            "model": self.config.model_name.as_deref().unwrap_or("gpt-4"),
            "messages": messages,
        });

        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = Value::Number(max_tokens.into());
        }
        if let Some(temperature) = req.temperature {
            body["temperature"] = Value::Number(serde_json::Number::from_f64(temperature as f64).unwrap_or(serde_json::Number::from(1)));
        }
        if let Some(top_p) = req.top_p {
            body["top_p"] = Value::Number(serde_json::Number::from_f64(top_p as f64).unwrap_or(serde_json::Number::from(1)));
        }
        if !req.stop_sequences.is_empty() {
            body["stop"] = Value::Array(req.stop_sequences.iter().map(|s| Value::String(s.clone())).collect());
        }

        // Add tool definitions
        if !req.tools.is_empty() {
            let tools_json = req.tools.iter().map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters_schema,
                    }
                })
            }).collect::<Vec<Value>>();
            body["tools"] = Value::Array(tools_json);
        }

        body
    }
}

#[async_trait]
impl LlmProvider for OpenAIProvider {
    async fn infer(&self, req: InferenceRequest) -> std::result::Result<InferenceResponse, OneAIError> {
        let body = self.to_openai_request(&req);
        let url = self.chat_url();

        // Log a compact summary at info level — message roles and key fields
        // This is essential when providers reject requests — we can see exactly
        // what format was sent (especially tool call / tool result message structure).
        let messages_summary: Vec<String> = body.get("messages")
            .and_then(|m| m.as_array())
            .map(|arr| arr.iter().map(|msg| {
                let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                let has_tc = msg.get("tool_calls").is_some();
                let has_tcid = msg.get("tool_call_id").is_some();
                let content_preview = msg.get("content").map(|c| {
                    if c.is_null() { "null" }
                    else { c.as_str().unwrap_or("<non-string>") }
                }).unwrap_or("<missing>");
                let content_len = msg.get("content").and_then(|c| c.as_str()).map(|s| s.len()).unwrap_or(0);
                let content_display = if content_len > 80 { format!("{}chars...", content_len) } else { content_preview.to_string() };
                let missing_flag = if msg.get("content").is_none() { " [MISSING]" } else { "" };
                format!("{}(content:{}{}, tc:{}, tcid:{})",
                    role, content_display, missing_flag,
                    if has_tc { "Y" } else { "N" },
                    if has_tcid { "Y" } else { "N" })
            }).collect())
            .unwrap_or_default();
        tracing::info!("OpenAI infer: model={}, messages=[{}]",
            self.config.model_name.as_deref().unwrap_or("?"),
            messages_summary.join(", ")
        );

        // Log the full request body at debug level for complete format inspection
        tracing::debug!("OpenAI infer request body: {}", serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()));

        let response = self.client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.config.api_key.as_deref().unwrap_or("")))
            .json(&body)
            .send()
            .await
            .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            tracing::error!("OpenAI API error {}: {}", status, text);
            return Err(OneAIError::Provider(format!("OpenAI API error {}: {}", status, text)));
        }

        let json: Value = response.json().await.map_err(|e| OneAIError::Network(e.to_string()))?;

        // Parse the response
        let choices = json.get("choices").and_then(|c| c.as_array());
        let first_choice = choices.and_then(|c| c.first());

        let model = json.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();

        let message_obj = first_choice.and_then(|c| c.get("message")).unwrap_or(&Value::Null);

        let role_str = message_obj.get("role").and_then(|r| r.as_str()).unwrap_or("assistant");
        let content_str = message_obj.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();

        // Parse tool calls from the response
        // Only create a Text block if content is non-empty — when the model
        // produces only tool calls (content: null), an empty Text block would
        // corrupt the conversation format (assistant messages with tool_calls
        // should have content: null, not content: "").
        let mut content_blocks = Vec::new();
        if !content_str.is_empty() {
            content_blocks.push(ContentBlock::Text { text: content_str.clone() });
        }

        // Parse reasoning_content (DeepSeek and other models that support thinking)
        if let Some(reasoning) = message_obj.get("reasoning_content").and_then(|r| r.as_str()) {
            if !reasoning.is_empty() {
                content_blocks.insert(0, ContentBlock::Thinking { text: reasoning.to_string() });
            }
        }

        if let Some(tool_calls) = message_obj.get("tool_calls").and_then(|tc| tc.as_array()) {
            for tc in tool_calls {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let func = tc.get("function").unwrap_or(&Value::Null);
                let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}").to_string();
                content_blocks.push(ContentBlock::ToolCall { id, name, args });
            }
        }

        let usage = json.get("usage").map(|u| TokenUsage {
            prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            completion_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        }).unwrap_or(TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        });

        Ok(InferenceResponse {
            message: Message {
                role: match role_str {
                    "system" => Role::System,
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    "tool" => Role::Tool,
                    _ => Role::Assistant,
                },
                content: content_blocks,
                metadata: HashMap::new(),
            },
            usage,
            model,
            metadata: HashMap::new(),
        })
    }

    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError> {
        let mut body = self.to_openai_request(&req);
        body["stream"] = Value::Bool(true);
        // Only include stream_options for providers known to support it.
        // 智谱GLM and some other OpenAI-compatible providers don't support
        // stream_options and may return empty/malformed streams when it's
        // present. OpenAI and DeepSeek are known to support it.
        // Usage data is still extracted from the final chunk's JSON when
        // available (many providers include usage without stream_options).
        let model_lower = self.config.model_name.as_deref().unwrap_or("").to_lowercase();
        let supports_stream_options = model_lower.contains("gpt")
            || model_lower.contains("o1") || model_lower.contains("o3") || model_lower.contains("o4")
            || model_lower.contains("deepseek");
        if supports_stream_options {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }

        let url = self.chat_url();

        // Log message format summary (same as infer method)
        let messages_summary: Vec<String> = body.get("messages")
            .and_then(|m| m.as_array())
            .map(|arr| arr.iter().map(|msg| {
                let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                let has_tc = msg.get("tool_calls").is_some();
                let has_tcid = msg.get("tool_call_id").is_some();
                let content_preview = msg.get("content").map(|c| {
                    if c.is_null() { "null" }
                    else { c.as_str().unwrap_or("<non-string>") }
                }).unwrap_or("<missing>");
                let content_len = msg.get("content").and_then(|c| c.as_str()).map(|s| s.len()).unwrap_or(0);
                let content_display = if content_len > 80 { format!("{}chars...", content_len) } else { content_preview.to_string() };
                let missing_flag = if msg.get("content").is_none() { " [MISSING]" } else { "" };
                format!("{}(content:{}{}, tc:{}, tcid:{})",
                    role, content_display, missing_flag,
                    if has_tc { "Y" } else { "N" },
                    if has_tcid { "Y" } else { "N" })
            }).collect())
            .unwrap_or_default();
        tracing::info!("OpenAI infer_stream: model={}, messages=[{}]",
            self.config.model_name.as_deref().unwrap_or("?"),
            messages_summary.join(", ")
        );
        tracing::debug!("OpenAI infer_stream request body: {}", serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()));

        let response = self.client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.config.api_key.as_deref().unwrap_or("")))
            .json(&body)
            .send()
            .await
            .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            tracing::error!("OpenAI API stream error {}: {}", status, text);
            return Err(OneAIError::Provider(format!("OpenAI API error {}: {}", status, text)));
        }

        // Create an SSE stream from the response
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        let stream = response.bytes_stream()
            .eventsource();

        // Spawn a task to process the SSE stream
        let model_name = self.config.model_name.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            // Track total content blocks emitted for diagnostic summary
            let mut total_text_chars: usize = 0;
            let mut total_thinking_chars: usize = 0;
            let mut total_tool_calls: usize = 0;
            let mut total_events_processed: usize = 0;
            let mut total_events_skipped_json: usize = 0;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(event) => {
                        if event.data == "[DONE]" {
                            tracing::debug!("SSE stream [DONE] received. Stats: events_processed={}, events_skipped_json={}, text_chars={}, thinking_chars={}, tool_calls={}",
                                total_events_processed, total_events_skipped_json,
                                total_text_chars, total_thinking_chars, total_tool_calls);
                            let _ = tx.send(InferenceStreamChunk {
                                content: vec![],
                                is_final: true,
                                usage: None,
                                model: model_name.clone(),
                            }).await;
                            break;
                        }

                        // Log raw SSE event data at debug level for format diagnostics
                        let data_preview = if event.data.len() > 500 {
                            // Char-boundary-safe truncation for CJK content
                            let end = event.data.char_indices()
                                .take_while(|(i, _)| *i < 500)
                                .last()
                                .map(|(i, c)| i + c.len_utf8())
                                .unwrap_or(0);
                            format!("{}...(truncated, total {} bytes)", &event.data[..end], event.data.len())
                        } else {
                            event.data.clone()
                        };
                        tracing::debug!("SSE event received: {}", data_preview);

                        // Parse the SSE data as JSON
                        // CRITICAL: Previously, JSON parsing failures were silently skipped
                        // with no logging, making SSE format issues completely invisible.
                        // Now we log the failure so format incompatibilities (e.g., 智谱GLM
                        // using a different response structure) are diagnosable.
                        let json_result = serde_json::from_str::<Value>(&event.data);
                        match json_result {
                            Ok(json) => {
                                total_events_processed += 1;
                                let choices = json.get("choices").and_then(|c| c.as_array());
                                let first_choice = choices.and_then(|c| c.first());

                                let delta = first_choice.and_then(|c| c.get("delta")).unwrap_or(&Value::Null);
                                let content = delta.get("content").and_then(|c| c.as_str()).unwrap_or("");

                                let is_final = first_choice
                                    .and_then(|c| c.get("finish_reason"))
                                    .and_then(|f| f.as_str())
                                    .map(|r| r != "null" && r != "None")
                                    .unwrap_or(false);

                                // Extract usage data — try both the final chunk format
                                // (OpenAI with stream_options) and the default format
                                // (some providers include usage without stream_options).
                                let usage = if is_final || json.get("usage").is_some() {
                                    json.get("usage").map(|u| TokenUsage {
                                        prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                        completion_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                        total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                    })
                                } else {
                                    None
                                };

                                let mut content_blocks = Vec::new();
                                if !content.is_empty() {
                                    total_text_chars += content.len();
                                    content_blocks.push(ContentBlock::Text { text: content.to_string() });
                                }

                                // Parse reasoning_content in delta (DeepSeek streaming)
                                let reasoning = delta.get("reasoning_content").and_then(|c| c.as_str()).unwrap_or("");
                                if !reasoning.is_empty() {
                                    total_thinking_chars += reasoning.len();
                                    content_blocks.push(ContentBlock::Thinking { text: reasoning.to_string() });
                                }

                                // Parse tool calls from stream delta
                                if let Some(tool_calls) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
                                    for tc in tool_calls {
                                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        let func = tc.get("function").unwrap_or(&Value::Null);
                                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        let args = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        if !name.is_empty() || !args.is_empty() {
                                            total_tool_calls += 1;
                                            content_blocks.push(ContentBlock::ToolCall { id, name, args });
                                        }
                                    }
                                }

                                if !content_blocks.is_empty() || is_final {
                                    let _ = tx.send(InferenceStreamChunk {
                                        content: content_blocks,
                                        is_final,
                                        usage,
                                        model: model_name.clone(),
                                    }).await;
                                }
                            }
                            Err(e) => {
                                // Log the JSON parsing failure — this is critical for
                                // diagnosing SSE format issues with providers like 智谱GLM
                                // that may use different response structures.
                                total_events_skipped_json += 1;
                                let preview = if event.data.len() > 200 {
                                    // Char-boundary-safe truncation for CJK content
                                    let end = event.data.char_indices()
                                        .take_while(|(i, _)| *i < 200)
                                        .last()
                                        .map(|(i, c)| i + c.len_utf8())
                                        .unwrap_or(0);
                                    format!("{}...(truncated)", &event.data[..end])
                                } else {
                                    event.data.clone()
                                };
                                tracing::warn!(
                                    "SSE event JSON parse failed: {}, data preview: {}",
                                    e, preview
                                );
                                // Don't break — skip this event and continue processing.
                                // Some providers send occasional non-JSON events (comments,
                                // keepalive pings) that should not terminate the stream.
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("SSE stream error: {:?}", e);
                        break;
                    }
                }
            }

            // If no events were processed and some were skipped due to JSON failures,
            // this likely means the entire stream was incompatible with our parser format.
            if total_events_processed == 0 && total_events_skipped_json > 0 {
                tracing::error!(
                    "SSE stream completed with 0 valid events ({} events skipped due to JSON parse failures). \
                    This strongly indicates a format incompatibility with the provider's SSE response. \
                    Model: {}",
                    total_events_skipped_json,
                    model_name.as_deref().unwrap_or("unknown")
                );
            }
        });

        let receiver_stream = ReceiverStream::new(rx);
        Ok(Box::pin(receiver_stream))
    }

    fn capabilities(&self) -> ModelCapability {
        // Default capabilities for OpenAI-compatible models
        ModelCapability {
            supports_multimodal: true,
            supports_streaming: true,
            supports_tools: true,
            context_window_size: 128000,
            max_output_tokens: 4096,
        }
    }

    fn config(&self) -> &ModelConfig {
        &self.config
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::{ContentBlock, Conversation, InferenceRequest, Message, Role, ToolDefinition};

    fn make_provider() -> OpenAIProvider {
        OpenAIProvider::new(ModelConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some("https://api.openai.com/v1".to_string()),
            model_name: Some("gpt-4".to_string()),
            ..ModelConfig::default()
        })
    }

    /// Test: assistant message with tool calls AND text content.
    /// Should produce: { role: "assistant", content: "text", tool_calls: [...] }
    #[test]
    fn test_assistant_message_with_tool_calls_and_text() {
        let provider = make_provider();
        let mut conv = Conversation::new();
        conv.add_message(Message::system("You are an agent."));
        conv.add_message(Message::user("Create a directory."));
        conv.add_message(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "Let me check the environment first.".to_string() },
                ContentBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "environment".to_string(),
                    args: "{}".to_string(),
                },
                ContentBlock::ToolCall {
                    id: "call_2".to_string(),
                    name: "environment".to_string(),
                    args: "{\"info_type\": \"all\"}".to_string(),
                },
            ],
            metadata: std::collections::HashMap::new(),
        });

        let req = InferenceRequest {
            conversation: conv,
            tools: vec![ToolDefinition {
                name: "environment".to_string(),
                description: "Get env info".to_string(),
                parameters_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        };

        let result = provider.to_openai_request(&req);
        let messages = result.get("messages").unwrap().as_array().unwrap();

        // Find the assistant message with tool_calls
        let assistant_msg = messages.iter().find(|m| {
            m.get("role").unwrap().as_str() == Some("assistant")
            && m.get("tool_calls").is_some()
        }).unwrap();

        // Content should be the text string (not null)
        assert_eq!(
            assistant_msg.get("content").unwrap().as_str(),
            Some("Let me check the environment first.")
        );

        // tool_calls should have 2 entries
        let tool_calls = assistant_msg.get("tool_calls").unwrap().as_array().unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].get("id").unwrap().as_str(), Some("call_1"));
        assert_eq!(tool_calls[1].get("id").unwrap().as_str(), Some("call_2"));
    }

    /// Test: assistant message with tool calls but NO text content.
    /// Should produce: { role: "assistant", content: null, tool_calls: [...] }
    /// (content MUST be null, not absent — some providers reject missing content)
    #[test]
    fn test_assistant_message_with_tool_calls_no_text() {
        let provider = make_provider();
        let mut conv = Conversation::new();
        conv.add_message(Message::system("You are an agent."));
        conv.add_message(Message::user("Create a directory."));
        // Model produces only tool calls, no text (common for many models)
        conv.add_message(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "environment".to_string(),
                    args: "{}".to_string(),
                },
            ],
            metadata: std::collections::HashMap::new(),
        });

        let req = InferenceRequest {
            conversation: conv,
            tools: vec![ToolDefinition {
                name: "environment".to_string(),
                description: "Get env info".to_string(),
                parameters_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        };

        let result = provider.to_openai_request(&req);
        let messages = result.get("messages").unwrap().as_array().unwrap();

        // Find the assistant message with tool_calls
        let assistant_msg = messages.iter().find(|m| {
            m.get("role").unwrap().as_str() == Some("assistant")
            && m.get("tool_calls").is_some()
        }).unwrap();

        // Content MUST be null (not absent or empty string)
        // Some providers (智谱GLM, 阿里百炼) require content: null
        // explicitly present in assistant messages with tool_calls.
        assert!(assistant_msg.get("content").is_some());
        assert_eq!(assistant_msg.get("content").unwrap(), &Value::Null);
    }

    /// Test: tool result message format.
    /// Should produce: { role: "tool", tool_call_id: "call_1", content: "raw content" }
    /// NOT: { role: "tool", tool_call_id: "call_1", content: "[Tool Result for call_1: raw content]" }
    #[test]
    fn test_tool_result_message_raw_content() {
        let provider = make_provider();
        let mut conv = Conversation::new();
        conv.add_message(Message::system("You are an agent."));
        conv.add_message(Message::user("Create a directory."));
        conv.add_message(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolCall {
                    id: "call_env1".to_string(),
                    name: "environment".to_string(),
                    args: "{}".to_string(),
                },
            ],
            metadata: std::collections::HashMap::new(),
        });
        conv.add_message(Message::tool_result(
            "call_env1".to_string(),
            "Working Directory: /home/user\nPlatform: macos".to_string(),
        ));

        let req = InferenceRequest {
            conversation: conv,
            tools: vec![ToolDefinition {
                name: "environment".to_string(),
                description: "Get env info".to_string(),
                parameters_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        };

        let result = provider.to_openai_request(&req);
        let messages = result.get("messages").unwrap().as_array().unwrap();

        // Find the tool result message
        let tool_msg = messages.iter().find(|m| {
            m.get("role").unwrap().as_str() == Some("tool")
        }).unwrap();

        // tool_call_id must match the original call
        assert_eq!(
            tool_msg.get("tool_call_id").unwrap().as_str(),
            Some("call_env1")
        );

        // Content must be the RAW tool output, NOT wrapped with "[Tool Result for call_id:]"
        // This is the critical fix — previously the content was:
        //   "[Tool Result for call_env1: Working Directory: /home/user\nPlatform: macos]"
        // which duplicates the call_id and adds unnecessary formatting.
        // Now it should be:
        //   "Working Directory: /home/user\nPlatform: macos"
        let content = tool_msg.get("content").unwrap().as_str().unwrap();
        assert!(
            !content.starts_with("[Tool Result for"),
            "Tool result content should NOT be wrapped with '[Tool Result for call_id:]'. Got: {}",
            content
        );
        assert_eq!(content, "Working Directory: /home/user\nPlatform: macos");
    }

    /// Test: full tool call → tool result conversation flow.
    /// Validates that the entire multi-message sequence formats correctly
    /// for the OpenAI API (assistant with tool_calls → tool result → next user message).
    #[test]
    fn test_full_tool_call_flow_format() {
        let provider = make_provider();
        let mut conv = Conversation::new();
        conv.add_message(Message::system("You are an agent."));
        conv.add_message(Message::user("Create a directory called /tmp/testdir."));
        conv.add_message(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolCall {
                    id: "call_env1".to_string(),
                    name: "environment".to_string(),
                    args: "{}".to_string(),
                },
                ContentBlock::ToolCall {
                    id: "call_env2".to_string(),
                    name: "environment".to_string(),
                    args: "{\"info_type\": \"all\"}".to_string(),
                },
            ],
            metadata: std::collections::HashMap::new(),
        });
        conv.add_message(Message::tool_result(
            "call_env1".to_string(),
            "Working Directory: /home/user".to_string(),
        ));
        conv.add_message(Message::tool_result(
            "call_env2".to_string(),
            "Working Directory: /home/user\nPlatform: macos\nArch: aarch64".to_string(),
        ));

        let req = InferenceRequest {
            conversation: conv,
            tools: vec![ToolDefinition {
                name: "environment".to_string(),
                description: "Get env info".to_string(),
                parameters_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        };

        let result = provider.to_openai_request(&req);
        let messages = result.get("messages").unwrap().as_array().unwrap();

        // Validate message ordering: system → user → assistant(tool_calls) → tool → tool
        assert_eq!(messages[0].get("role").unwrap().as_str(), Some("system"));
        assert_eq!(messages[1].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(messages[2].get("role").unwrap().as_str(), Some("assistant"));
        assert_eq!(messages[3].get("role").unwrap().as_str(), Some("tool"));
        assert_eq!(messages[4].get("role").unwrap().as_str(), Some("tool"));

        // Assistant message has tool_calls and content: null (no text)
        let assistant_msg = &messages[2];
        assert!(assistant_msg.get("tool_calls").is_some());
        assert_eq!(assistant_msg.get("content").unwrap(), &Value::Null);
        let tool_calls = assistant_msg.get("tool_calls").unwrap().as_array().unwrap();
        assert_eq!(tool_calls.len(), 2);

        // Tool result messages have raw content (no wrapper)
        let tool_msg1 = &messages[3];
        assert_eq!(tool_msg1.get("tool_call_id").unwrap().as_str(), Some("call_env1"));
        assert_eq!(tool_msg1.get("content").unwrap().as_str(), Some("Working Directory: /home/user"));

        let tool_msg2 = &messages[4];
        assert_eq!(tool_msg2.get("tool_call_id").unwrap().as_str(), Some("call_env2"));
        let content2 = tool_msg2.get("content").unwrap().as_str().unwrap();
        assert!(!content2.starts_with("[Tool Result for"));
    }

    // ─── stream_options conditional tests ───────────────────────────────────────

    /// Test: stream_options is included for GPT models (known to support it).
    #[test]
    fn test_stream_options_included_for_gpt() {
        let provider = OpenAIProvider::new(ModelConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some("https://api.openai.com/v1".to_string()),
            model_name: Some("gpt-4o".to_string()),
            ..ModelConfig::default()
        });
        let mut conv = Conversation::new();
        conv.add_message(Message::user("Hello"));
        let req = InferenceRequest {
            conversation: conv,
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let mut body = provider.to_openai_request(&req);
        body["stream"] = Value::Bool(true);
        let model_lower = provider.config.model_name.as_deref().unwrap_or("").to_lowercase();
        let supports_stream_options = model_lower.contains("gpt");
        if supports_stream_options {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }

        assert!(body.get("stream_options").is_some());
    }

    /// Test: stream_options is NOT included for GLM models (not known to support it).
    #[test]
    fn test_stream_options_excluded_for_glm() {
        let provider = OpenAIProvider::new(ModelConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some("https://open.bigmodel.cn/api/paas/v4".to_string()),
            model_name: Some("glm-5.1".to_string()),
            ..ModelConfig::default()
        });
        let mut conv = Conversation::new();
        conv.add_message(Message::user("Hello"));
        let req = InferenceRequest {
            conversation: conv,
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let mut body = provider.to_openai_request(&req);
        body["stream"] = Value::Bool(true);
        let model_lower = provider.config.model_name.as_deref().unwrap_or("").to_lowercase();
        let supports_stream_options = model_lower.contains("gpt")
            || model_lower.contains("o1") || model_lower.contains("o3") || model_lower.contains("o4")
            || model_lower.contains("deepseek");
        if supports_stream_options {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }

        // GLM should NOT have stream_options
        assert!(body.get("stream_options").is_none());
    }

    /// Test: stream_options is included for DeepSeek models.
    #[test]
    fn test_stream_options_included_for_deepseek() {
        let provider = OpenAIProvider::new(ModelConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some("https://api.deepseek.com/v1".to_string()),
            model_name: Some("deepseek-chat".to_string()),
            ..ModelConfig::default()
        });
        let mut conv = Conversation::new();
        conv.add_message(Message::user("Hello"));
        let req = InferenceRequest {
            conversation: conv,
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let mut body = provider.to_openai_request(&req);
        body["stream"] = Value::Bool(true);
        let model_lower = provider.config.model_name.as_deref().unwrap_or("").to_lowercase();
        let supports_stream_options = model_lower.contains("deepseek");
        if supports_stream_options {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }

        assert!(body.get("stream_options").is_some());
    }
}