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
            let mut openai_msg = serde_json::json!({
                "role": match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                },
            });

            // Handle content blocks
            let has_tool_calls = msg.tool_calls().is_empty();
            if !has_tool_calls {
                // Extract tool calls
                let tool_calls: Vec<Value> = msg.content.iter()
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

                openai_msg["tool_calls"] = Value::Array(tool_calls);
            }

            // Build content
            let text_parts: Vec<String> = msg.content.iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.clone()),
                    ContentBlock::ToolResult { call_id, content } => {
                        Some(format!("[Tool Result for {}: {}]", call_id, content))
                    }
                    _ => None,
                })
                .collect();

            if !text_parts.is_empty() {
                openai_msg["content"] = Value::String(text_parts.join("\n"));
            }

            // Add tool_call_id for tool result messages
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
        let mut content_blocks = vec![ContentBlock::Text { text: content_str.clone() }];

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
        // Request usage data in streaming response (OpenAI requires stream_options)
        body["stream_options"] = serde_json::json!({"include_usage": true});

        let url = self.chat_url();

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
            while let Some(event) = stream.next().await {
                match event {
                    Ok(event) => {
                        if event.data == "[DONE]" {
                            let _ = tx.send(InferenceStreamChunk {
                                content: vec![],
                                is_final: true,
                                usage: None,
                                model: model_name.clone(),
                            }).await;
                            break;
                        }

                        // Parse the SSE data as JSON
                        if let Ok(json) = serde_json::from_str::<Value>(&event.data) {
                            let choices = json.get("choices").and_then(|c| c.as_array());
                            let first_choice = choices.and_then(|c| c.first());

                            let delta = first_choice.and_then(|c| c.get("delta")).unwrap_or(&Value::Null);
                            let content = delta.get("content").and_then(|c| c.as_str()).unwrap_or("");

                            let is_final = first_choice
                                .and_then(|c| c.get("finish_reason"))
                                .and_then(|f| f.as_str())
                                .map(|r| r != "null" && r != "None")
                                .unwrap_or(false);

                            let usage = if is_final {
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
                                content_blocks.push(ContentBlock::Text { text: content.to_string() });
                            }

                            // Parse reasoning_content in delta (DeepSeek streaming)
                            let reasoning = delta.get("reasoning_content").and_then(|c| c.as_str()).unwrap_or("");
                            if !reasoning.is_empty() {
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
                    }
                    Err(e) => {
                        tracing::error!("SSE stream error: {:?}", e);
                        break;
                    }
                }
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

