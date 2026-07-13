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

        // Constrained/structured output — Ollama's OpenAI-compatible endpoint
        // accepts `format: { type: "json_schema", schema }` to grammar-constrain
        // decoding. This is the Layer-1 path that matters for local small models,
        // where unconstrained JSON emission fails often enough to be worth the
        // quality tradeoff. Only attached when the agent's tier-gating policy
        // opted in (see AgentLoop::build_constrained_output).
        if let Some(cfg) = &req.constrained_output {
            body["format"] = serde_json::json!({
                "type": "json_schema",
                "schema": cfg.schema,
            });
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
            ..Default::default()}).unwrap_or(TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            ..Default::default()});

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

    /// L2 probe: query Ollama's `/api/show` for the model's context length.
    ///
    /// Ollama returns a `model_info` object whose keys are architecture-scoped
    /// (`llama.context_length`, `qwen2.context_length`, `gemma.context_length`,
    /// `command_r.context_length`, …). Different model families use different
    /// key prefixes, so we scan for any key ending in `.context_length`.
    ///
    /// Best-effort: any network/parse failure returns `None` so the resolver
    /// falls through to the built-in library without blocking inference.
    async fn probe_context_window(&self) -> Option<u32> {
        let model = self.config.model_name.as_deref()?;
        let base = self.config.resolved_url();
        let url = format!("{}/api/show", base.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "model": model }))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: Value = resp.json().await.ok()?;
        parse_ollama_context_window(&json)
    }

    fn config(&self) -> &ModelConfig {
        &self.config
    }

    /// Ollama serves local models — constrained decoding is net positive here.
    fn prefers_constrained_output(&self) -> bool {
        true
    }
}

// ─── Ollama /api/show context-length parser ─────────────────────────────────

/// Parse the context-window size from an Ollama `/api/show` response.
///
/// Scans the `model_info` object for any key ending in `context_length`
/// (e.g. `llama.context_length`, `qwen2.context_length`, `gemma.context_length`)
/// and returns the first non-zero value found. Returns `None` if absent.
pub fn parse_ollama_context_window(json: &Value) -> Option<u32> {
    let model_info = json.get("model_info")?;
    let obj = model_info.as_object()?;
    for (key, val) in obj {
        if key.ends_with("context_length") {
            if let Some(n) = val.as_u64() {
                if n > 0 {
                    // Ollama reports context_length as a number; cap at u32 max.
                    return Some(n.min(u32::MAX as u64) as u32);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_ollama_llama_context() {
        let resp = json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.context_length": 8192,
                "llama.embedding_length": 4096,
            }
        });
        assert_eq!(parse_ollama_context_window(&resp), Some(8192));
    }

    #[test]
    fn test_parse_ollama_qwen_context() {
        let resp = json!({
            "model_info": {
                "general.architecture": "qwen2",
                "qwen2.context_length": 32768,
            }
        });
        assert_eq!(parse_ollama_context_window(&resp), Some(32768));
    }

    #[test]
    fn test_parse_ollama_missing_model_info() {
        let resp = json!({ "license": "mit" });
        assert_eq!(parse_ollama_context_window(&resp), None);
    }

    #[test]
    fn test_parse_ollama_no_context_length_key() {
        let resp = json!({ "model_info": { "general.architecture": "llama" } });
        assert_eq!(parse_ollama_context_window(&resp), None);
    }
}

#[cfg(test)]
mod constrained_tests {
    use super::*;
    use oneai_core::{ConstrainedMode, ConstrainedOutputConfig, Conversation, InferenceRequest};
    use serde_json::json;

    fn provider() -> OllamaProvider {
        OllamaProvider::new(ModelConfig::ollama("llama3".to_string()))
    }

    fn request(constrained: Option<ConstrainedOutputConfig>) -> InferenceRequest {
        InferenceRequest {
            conversation: Conversation::new(),
            tools: Vec::new(),
            max_tokens: Some(64),
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            constrained_output: constrained,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn ollama_prefers_constrained_output() {
        // Local backend — constrained decoding is net positive.
        assert!(provider().prefers_constrained_output());
    }

    #[test]
    fn to_ollama_request_emits_format_when_constrained() {
        let cfg = ConstrainedOutputConfig {
            schema: json!({ "type": "object", "required": ["answer"] }),
            mode: ConstrainedMode::JsonSchema,
        };
        let body = provider().to_ollama_request(&request(Some(cfg)));
        assert_eq!(body["format"]["type"], "json_schema");
        assert_eq!(
            body["format"]["schema"]["required"][0],
            "answer"
        );
    }

    #[test]
    fn to_ollama_request_omits_format_when_unset() {
        let body = provider().to_ollama_request(&request(None));
        assert!(body.get("format").is_none());
    }
}

