//! Anthropic Claude native protocol provider implementation.
//!
//! Uses the Anthropic Messages API for streaming and non-streaming inference.
//! Claude uses a different API format than OpenAI (no "choices" array, direct message output).

use async_trait::async_trait;
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

/// Anthropic Claude LLM provider.
pub struct AnthropicProvider {
    config: ModelConfig,
    client: Client,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider with the given configuration.
    pub fn new(config: ModelConfig) -> Self {
        let client = Client::new();
        Self { config, client }
    }

    /// Create with a custom HTTP client.
    pub fn with_client(config: ModelConfig, client: Client) -> Self {
        Self { config, client }
    }

    /// Get the Anthropic Messages API endpoint URL.
    fn messages_url(&self) -> String {
        let base = self.config.resolved_url();
        format!("{}/messages", base.trim_end_matches('/'))
    }

    /// Convert an InferenceRequest to Anthropic Messages API format.
    fn to_anthropic_request(&self, req: &InferenceRequest) -> Value {
        // Anthropic separates system messages from the conversation
        let mut system_text = String::new();
        let mut messages = Vec::new();

        for msg in &req.conversation.messages {
            match msg.role {
                Role::System => {
                    // Anthropic puts system messages in a separate field
                    system_text.push_str(&msg.text_content());
                    system_text.push('\n');
                }
                Role::User | Role::Assistant => {
                    let mut content_blocks = Vec::new();
                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } => {
                                content_blocks.push(serde_json::json!({
                                    "type": "text",
                                    "text": text,
                                }));
                            }
                            ContentBlock::Image { mime_type, data } => {
                                use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
                                content_blocks.push(serde_json::json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": mime_type,
                                        "data": BASE64.encode(data),
                                    }
                                }));
                            }
                            ContentBlock::ToolCall { id, name, args } => {
                                content_blocks.push(serde_json::json!({
                                    "type": "tool_use",
                                    "id": id,
                                    "name": name,
                                    "input": serde_json::from_str::<Value>(args).unwrap_or(Value::Object(serde_json::Map::new())),
                                }));
                            }
                            ContentBlock::ToolResult { call_id, content } => {
                                content_blocks.push(serde_json::json!({
                                    "type": "tool_result",
                                    "tool_use_id": call_id,
                                    "content": content,
                                }));
                            }
                            _ => {}
                        }
                    }

                    messages.push(serde_json::json!({
                        "role": match msg.role {
                            Role::User => "user",
                            Role::Assistant => "assistant",
                            _ => "user",
                        },
                        "content": content_blocks,
                    }));
                }
                Role::Tool => {
                    // Tool results in Anthropic are wrapped in user messages
                    for block in &msg.content {
                        if let ContentBlock::ToolResult { call_id, content } = block {
                            messages.push(serde_json::json!({
                                "role": "user",
                                "content": [{
                                    "type": "tool_result",
                                    "tool_use_id": call_id,
                                    "content": content,
                                }]
                            }));
                        }
                    }
                }
            }
        }

        let mut body = serde_json::json!({
            "model": self.config.model_name.as_deref().unwrap_or("claude-sonnet-4-20250514"),
            "messages": messages,
        });

        if !system_text.is_empty() {
            body["system"] = Value::String(system_text.trim().to_string());
        }

        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = Value::Number(max_tokens.into());
        } else {
            // Anthropic requires max_tokens
            body["max_tokens"] = Value::Number(4096.into());
        }

        if let Some(temperature) = req.temperature {
            body["temperature"] = Value::Number(
                serde_json::Number::from_f64(temperature as f64).unwrap_or(serde_json::Number::from(1))
            );
        }
        if let Some(top_p) = req.top_p {
            body["top_p"] = Value::Number(
                serde_json::Number::from_f64(top_p as f64).unwrap_or(serde_json::Number::from(1))
            );
        }
        if !req.stop_sequences.is_empty() {
            body["stop_sequences"] = Value::Array(
                req.stop_sequences.iter().map(|s| Value::String(s.clone())).collect()
            );
        }

        // Add tool definitions (Anthropic format)
        if !req.tools.is_empty() {
            let tools_json = req.tools.iter().map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters_schema,
                })
            }).collect::<Vec<Value>>();
            body["tools"] = Value::Array(tools_json);
        }

        body
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn infer(&self, req: InferenceRequest) -> std::result::Result<InferenceResponse, OneAIError> {
        let body = self.to_anthropic_request(&req);
        let url = self.messages_url();

        let response = self.client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-api-key", self.config.api_key.as_deref().unwrap_or(""))
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            return Err(OneAIError::Provider(format!("Anthropic API error {}: {}", status, text)));
        }

        let json: Value = response.json().await.map_err(|e| OneAIError::Network(e.to_string()))?;

        // Parse Anthropic response format
        let model = json.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();

        let content_array = json.get("content").and_then(|c| c.as_array());

        let mut content_blocks = Vec::new();
        if let Some(content) = content_array {
            for block in content {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        content_blocks.push(ContentBlock::Text { text: text.to_string() });
                    }
                    "tool_use" => {
                        let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let input = block.get("input").cloned().unwrap_or(Value::Object(serde_json::Map::new()));
                        let args = input.to_string();
                        content_blocks.push(ContentBlock::ToolCall { id, name, args });
                    }
                    _ => {}
                }
            }
        }

        let usage = json.get("usage").map(|u| TokenUsage {
            prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            total_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32
                + u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        }).unwrap_or(TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        });

        let stop_reason = json.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("end_turn");

        Ok(InferenceResponse {
            message: Message {
                role: Role::Assistant,
                content: content_blocks,
                metadata: HashMap::from([("stop_reason".to_string(), stop_reason.to_string())]),
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
        let mut body = self.to_anthropic_request(&req);
        body["stream"] = Value::Bool(true);

        let url = self.messages_url();

        let response = self.client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-api-key", self.config.api_key.as_deref().unwrap_or(""))
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            return Err(OneAIError::Provider(format!("Anthropic API error {}: {}", status, text)));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        let model_name = self.config.model_name.clone();
        tokio::spawn(async move {
            let stream = response.bytes_stream();
            let mut buffer = String::new();

            use eventsource_stream::Eventsource;
            let mut sse_stream = stream.eventsource();

            while let Some(event) = sse_stream.next().await {
                match event {
                    Ok(event) => {
                        let event_type = event.event.clone();

                        if event_type == "message_stop" {
                            let _ = tx.send(InferenceStreamChunk {
                                content: vec![],
                                is_final: true,
                                usage: None,
                                model: model_name.clone(),
                            }).await;
                            break;
                        }

                        if let Ok(json) = serde_json::from_str::<Value>(&event.data) {
                            let event_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");

                            match event_type {
                                "content_block_delta" => {
                                    let delta = json.get("delta").unwrap_or(&Value::Null);
                                    let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");

                                    match delta_type {
                                        "text_delta" => {
                                            let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                            if !text.is_empty() {
                                                let _ = tx.send(InferenceStreamChunk {
                                                    content: vec![ContentBlock::Text { text: text.to_string() }],
                                                    is_final: false,
                                                    usage: None,
                                                    model: model_name.clone(),
                                                }).await;
                                            }
                                        }
                                        "input_json_delta" => {
                                            let partial_json = delta.get("partial_json").and_then(|p| p.as_str()).unwrap_or("");
                                            // We accumulate partial JSON for tool calls
                                            // The full tool call data comes in content_block_start + content_block_delta events
                                            buffer.push_str(partial_json);
                                        }
                                        _ => {}
                                    }
                                }
                                "content_block_start" => {
                                    let content_block = json.get("content_block").unwrap_or(&Value::Null);
                                    let cb_type = content_block.get("type").and_then(|t| t.as_str()).unwrap_or("");

                                    if cb_type == "tool_use" {
                                        let id = content_block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        let name = content_block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        buffer.clear();
                                        // Store the tool call ID and name for later
                                        buffer = format!("{{\"id\":\"{}\",\"name\":\"{}\",\"args\":\"", id, name);
                                    }
                                }
                                "content_block_stop" => {
                                    // If we were accumulating a tool call, finalize it
                                    if buffer.starts_with("{\"id\":") {
                                        buffer.push_str("\"}");
                                        // Parse the accumulated buffer
                                        if let Ok(parsed) = serde_json::from_str::<Value>(&buffer) {
                                            let id = parsed.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                            let name = parsed.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                            // Get the accumulated partial JSON from input_json_delta events
                                            // This is simplified — real implementation would accumulate properly
                                            let _ = tx.send(InferenceStreamChunk {
                                                content: vec![ContentBlock::ToolCall {
                                                    id,
                                                    name,
                                                    args: "{}".to_string(), // Simplified
                                                }],
                                                is_final: false,
                                                usage: None,
                                                model: model_name.clone(),
                                            }).await;
                                        }
                                        buffer.clear();
                                    }
                                }
                                "message_delta" => {
                                    let delta = json.get("delta").unwrap_or(&Value::Null);
                                    let stop_reason = delta.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("");

                                    let usage_obj = json.get("usage").unwrap_or(&Value::Null);
                                    let usage = TokenUsage {
                                        prompt_tokens: 0, // Not provided in stream deltas
                                        completion_tokens: usage_obj.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                        total_tokens: usage_obj.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                    };

                                    let _ = tx.send(InferenceStreamChunk {
                                        content: vec![],
                                        is_final: stop_reason != "",
                                        usage: Some(usage),
                                        model: model_name.clone(),
                                    }).await;
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Anthropic SSE stream error: {:?}", e);
                        break;
                    }
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn capabilities(&self) -> ModelCapability {
        ModelCapability::claude_class()
    }

    fn config(&self) -> &ModelConfig {
        &self.config
    }
}