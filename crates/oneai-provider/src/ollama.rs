//! Ollama local provider implementation.
//!
//! Ollama uses an OpenAI-compatible API at /v1/chat/completions,
//! but also has its own native API at /api/chat and /api/generate.
//! This implementation uses the OpenAI-compatible endpoint for simplicity.

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

/// Ollama local LLM provider.
pub struct OllamaProvider {
    config: ModelConfig,
    client: Client,
}

impl OllamaProvider {
    /// Create a new Ollama provider with the given configuration.
    pub fn new(config: ModelConfig) -> Self {
        let client = Client::new();
        Self { config, client }
    }

    /// Get the Ollama chat completions endpoint URL.
    fn chat_url(&self) -> String {
        let base = self.config.resolved_url();
        format!("{}/v1/chat/completions", base.trim_end_matches('/'))
    }

    /// Convert an InferenceRequest to Ollama-compatible (OpenAI format) request.
    fn to_ollama_request(&self, req: &InferenceRequest) -> Value {
        let mut messages = Vec::new();
        for msg in &req.conversation.messages {
            let text = msg.text_content();
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                _ => "user", // #[non_exhaustive] catch-all
            };
            messages.push(serde_json::json!({
                "role": role,
                "content": text,
            }));
        }

        let mut body = serde_json::json!({
            "model": self.config.model_name.as_deref().unwrap_or("llama3"),
            "messages": messages,
        });

        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = Value::Number(max_tokens.into());
        }
        if let Some(temperature) = req.temperature {
            body["temperature"] = Value::Number(
                serde_json::Number::from_f64(temperature as f64).unwrap_or(serde_json::Number::from(1))
            );
        }

        // Add tool definitions (Ollama supports OpenAI-compatible tools)
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
impl LlmProvider for OllamaProvider {
    async fn infer(&self, req: InferenceRequest) -> std::result::Result<InferenceResponse, OneAIError> {
        let body = self.to_ollama_request(&req);
        let url = self.chat_url();

        let response = self.client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            return Err(OneAIError::Provider(format!("Ollama API error {}: {}", status, text)));
        }

        // Parse the OpenAI-compatible response
        let json: Value = response.json().await.map_err(|e| OneAIError::Network(e.to_string()))?;

        let model = json.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();

        let choices = json.get("choices").and_then(|c| c.as_array());
        let first_choice = choices.and_then(|c| c.first());

        let message_obj = first_choice.and_then(|c| c.get("message")).unwrap_or(&Value::Null);
        let content_str = message_obj.get("content").and_then(|c| c.as_str()).unwrap_or("");

        let mut content_blocks = vec![ContentBlock::Text { text: content_str.to_string() }];

        // Parse tool calls
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
                role: Role::Assistant,
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
        let mut body = self.to_ollama_request(&req);
        body["stream"] = Value::Bool(true);

        let url = self.chat_url();

        let response = self.client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            return Err(OneAIError::Provider(format!("Ollama API error {}: {}", status, text)));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let model_name = self.config.model_name.clone();

        let stream = response.bytes_stream().eventsource();
        tokio::spawn(async move {
            let mut sse_stream = stream;
            while let Some(event) = sse_stream.next().await {
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

                            let mut content_blocks = Vec::new();
                            if !content.is_empty() {
                                content_blocks.push(ContentBlock::Text { text: content.to_string() });
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
                                    usage: None,
                                    model: model_name.clone(),
                                }).await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Ollama SSE stream error: {:?}", e);
                        break;
                    }
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn capabilities(&self) -> ModelCapability {
        // Ollama capabilities depend on the model — use conservative defaults
        ModelCapability {
            supports_multimodal: false, // Depends on model
            supports_streaming: true,
            supports_tools: true,
            context_window_size: 8192, // Model-specific
            max_output_tokens: 4096,
        }
    }

    fn config(&self) -> &ModelConfig {
        &self.config
    }
}

