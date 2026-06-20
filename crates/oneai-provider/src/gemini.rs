//! Google Gemini provider implementation.
//!
//! Uses the Google AI API for Gemini model inference. The Gemini API has a
//! different format from both OpenAI and Anthropic:
//! - Uses `generateContent` endpoint for non-streaming
//! - Uses `streamGenerateContent` endpoint for streaming
//! - Content is structured as `parts` (text, functionCall, functionResponse)
//! - System instructions are in a separate `systemInstruction` field
//! - Function declarations use `FunctionDeclaration` format
//!
//! Configure via `ModelConfig::gemini(api_key, model_name)` or
//! `ModelConfig.extra["api_mode"] = "vertex_ai"` for Vertex AI endpoint.

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

/// Google Gemini LLM provider.
pub struct GeminiProvider {
    config: ModelConfig,
    client: Client,
}

impl GeminiProvider {
    /// Create a new Gemini provider with the given configuration.
    pub fn new(config: ModelConfig) -> Self {
        let client = Client::new();
        Self { config, client }
    }

    /// Create with a custom HTTP client.
    pub fn with_client(config: ModelConfig, client: Client) -> Self {
        Self { config, client }
    }

    /// Get the Gemini generateContent endpoint URL.
    fn generate_url(&self) -> String {
        let api_key = self.config.api_key.as_deref().unwrap_or("");
        let model = self.config.model_name.as_deref().unwrap_or("gemini-2.0-flash");

        // Check for Vertex AI mode
        if self.config.extra.get("api_mode").map(|s| s.as_str()) == Some("vertex_ai") {
            let region = self.config.extra.get("region").map(|s| s.as_str()).unwrap_or("us-central1");
            let project = self.config.extra.get("project").cloned().unwrap_or_else(|| "default".to_string());
            format!(
                "https://{}-aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/google/models/{}:generateContent",
                region, project, region, model
            )
        } else {
            format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
                model, api_key
            )
        }
    }

    /// Get the Gemini streamGenerateContent endpoint URL.
    fn stream_url(&self) -> String {
        let api_key = self.config.api_key.as_deref().unwrap_or("");
        let model = self.config.model_name.as_deref().unwrap_or("gemini-2.0-flash");

        if self.config.extra.get("api_mode").map(|s| s.as_str()) == Some("vertex_ai") {
            let region = self.config.extra.get("region").map(|s| s.as_str()).unwrap_or("us-central1");
            let project = self.config.extra.get("project").cloned().unwrap_or_else(|| "default".to_string());
            format!(
                "https://{}-aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/google/models/{}:streamGenerateContent?alt=sse",
                region, project, region, model
            )
        } else {
            format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
                model, api_key
            )
        }
    }

    /// Convert an InferenceRequest to Gemini API format.
    fn to_gemini_request(&self, req: &InferenceRequest) -> Value {
        // Gemini separates system instructions from the conversation
        let mut system_parts = Vec::new();
        let mut contents = Vec::new();

        for msg in &req.conversation.messages {
            match msg.role {
                Role::System => {
                    // System messages go into systemInstruction field
                    system_parts.push(serde_json::json!({
                        "text": msg.text_content(),
                    }));
                }
                Role::User => {
                    let mut parts = Vec::new();
                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } => {
                                parts.push(serde_json::json!({
                                    "text": text,
                                }));
                            }
                            ContentBlock::Image { mime_type, data } => {
                                use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
                                parts.push(serde_json::json!({
                                    "inline_data": {
                                        "mime_type": mime_type,
                                        "data": BASE64.encode(data),
                                    }
                                }));
                            }
                            ContentBlock::ToolResult { call_id, content } => {
                                // Gemini uses functionResponse for tool results
                                parts.push(serde_json::json!({
                                    "functionResponse": {
                                        "name": call_id,
                                        "response": {
                                            "content": content,
                                        }
                                    }
                                }));
                            }
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        contents.push(serde_json::json!({
                            "role": "user",
                            "parts": parts,
                        }));
                    }
                }
                Role::Assistant => {
                    let mut parts = Vec::new();
                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } => {
                                parts.push(serde_json::json!({
                                    "text": text,
                                }));
                            }
                            ContentBlock::ToolCall { id: _, name, args } => {
                                // Gemini uses functionCall for tool calls
                                let args_value = serde_json::from_str::<Value>(args)
                                    .unwrap_or(Value::Object(serde_json::Map::new()));
                                parts.push(serde_json::json!({
                                    "functionCall": {
                                        "name": name,
                                        "args": args_value,
                                    }
                                }));
                            }
                            ContentBlock::Thinking { text } => {
                                parts.push(serde_json::json!({
                                    "text": text,
                                }));
                            }
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        contents.push(serde_json::json!({
                            "role": "model", // Gemini uses "model" for assistant
                            "parts": parts,
                        }));
                    }
                }
                Role::Tool => {
                    // Tool results in Gemini are user-role functionResponse parts
                    for block in &msg.content {
                        if let ContentBlock::ToolResult { call_id, content } = block {
                            contents.push(serde_json::json!({
                                "role": "user",
                                "parts": [{
                                    "functionResponse": {
                                        "name": call_id,
                                        "response": {
                                            "content": content,
                                        }
                                    }
                                }]
                            }));
                        }
                    }
                }
                _ => {} // #[non_exhaustive] catch-all
            }
        }

        let mut body = serde_json::json!({
            "contents": contents,
        });

        // Add system instruction
        if !system_parts.is_empty() {
            body["systemInstruction"] = serde_json::json!({
                "parts": system_parts,
            });
        }

        // Generation config
        let mut generation_config = serde_json::json!({});
        if let Some(max_tokens) = req.max_tokens {
            generation_config["maxOutputTokens"] = Value::Number(max_tokens.into());
        } else {
            generation_config["maxOutputTokens"] = Value::Number(4096.into());
        }
        if let Some(temperature) = req.temperature {
            generation_config["temperature"] = Value::Number(
                serde_json::Number::from_f64(temperature as f64).unwrap_or(serde_json::Number::from(1))
            );
        }
        if let Some(top_p) = req.top_p {
            generation_config["topP"] = Value::Number(
                serde_json::Number::from_f64(top_p as f64).unwrap_or(serde_json::Number::from(1))
            );
        }
        if !req.stop_sequences.is_empty() {
            generation_config["stopSequences"] = Value::Array(
                req.stop_sequences.iter().map(|s| Value::String(s.clone())).collect()
            );
        }
        body["generationConfig"] = generation_config;

        // Add tool declarations (Gemini format)
        if !req.tools.is_empty() {
            let declarations: Vec<Value> = req.tools.iter().map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters_schema,
                })
            }).collect();

            body["tools"] = serde_json::json!([
                {
                    "function_declarations": declarations,
                }
            ]);
        }

        body
    }

    /// Parse a Gemini generateContent response.
    fn parse_response(json: &Value, model: &str) -> std::result::Result<InferenceResponse, OneAIError> {
        let candidates = json.get("candidates")
            .and_then(|c| c.as_array());

        let mut content_blocks = Vec::new();
        let mut finish_reason = "stop".to_string();

        if let Some(candidates) = candidates {
            if let Some(candidate) = candidates.first() {
                finish_reason = candidate.get("finishReason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("stop")
                    .to_string();

                let content = candidate.get("content");
                if let Some(content) = content {
                    let parts = content.get("parts").and_then(|p| p.as_array());
                    if let Some(parts) = parts {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                content_blocks.push(ContentBlock::Text { text: text.to_string() });
                            }
                            if let Some(fc) = part.get("functionCall") {
                                let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                let args = fc.get("args").cloned()
                                    .unwrap_or(Value::Object(serde_json::Map::new()));
                                // Gemini doesn't provide call IDs — generate one
                                let id = format!("call_{}", &uuid::Uuid::new_v4().to_string().replace("-", "")[..8]);
                                content_blocks.push(ContentBlock::ToolCall {
                                    id,
                                    name,
                                    args: args.to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        let usage = json.get("usageMetadata").map(|u| TokenUsage {
            prompt_tokens: u.get("promptTokenCount").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            completion_tokens: u.get("candidatesTokenCount").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            total_tokens: u.get("totalTokenCount").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        }).unwrap_or(TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        });

        Ok(InferenceResponse {
            message: Message {
                role: Role::Assistant,
                content: content_blocks,
                metadata: HashMap::from([("finish_reason".to_string(), finish_reason)]),
            },
            usage,
            model: model.to_string(),
            metadata: HashMap::new(),
        })
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn infer(&self, req: InferenceRequest) -> std::result::Result<InferenceResponse, OneAIError> {
        let body = self.to_gemini_request(&req);
        let url = self.generate_url();
        let model = self.config.model_name.as_deref().unwrap_or("gemini-2.0-flash").to_string();

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
            return Err(OneAIError::Provider(format!("Gemini API error {}: {}", status, text)));
        }

        let json: Value = response.json().await.map_err(|e| OneAIError::Network(e.to_string()))?;
        Self::parse_response(&json, &model)
    }

    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError> {
        let body = self.to_gemini_request(&req);
        let url = self.stream_url();
        let model_name = self.config.model_name.clone();

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
            return Err(OneAIError::Provider(format!("Gemini API error {}: {}", status, text)));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            let stream = response.bytes_stream();
            use eventsource_stream::Eventsource;
            let mut sse_stream = stream.eventsource();

            let mut prompt_tokens_from_start: u32 = 0;

            while let Some(event) = sse_stream.next().await {
                match event {
                    Ok(event) => {
                        if let Ok(json) = serde_json::from_str::<Value>(&event.data) {
                            // Gemini streaming returns each chunk as a generateContent response
                            // with incremental candidates

                            // Check for usageMetadata
                            if let Some(usage) = json.get("usageMetadata") {
                                prompt_tokens_from_start = usage.get("promptTokenCount")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0) as u32;
                            }

                            // Parse candidates
                            let candidates = json.get("candidates")
                                .and_then(|c| c.as_array());

                            if let Some(candidates) = candidates {
                                for candidate in candidates {
                                    let content = candidate.get("content");
                                    if let Some(content) = content {
                                        let parts = content.get("parts").and_then(|p| p.as_array());
                                        if let Some(parts) = parts {
                                            for part in parts {
                                                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                                    if !text.is_empty() {
                                                        let _ = tx.send(InferenceStreamChunk {
                                                            content: vec![ContentBlock::Text { text: text.to_string() }],
                                                            is_final: false,
                                                            usage: None,
                                                            model: model_name.clone(),
                                                        }).await;
                                                    }
                                                }
                                                if let Some(fc) = part.get("functionCall") {
                                                    let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                                    let args = fc.get("args").cloned()
                                                        .unwrap_or(Value::Object(serde_json::Map::new()));
                                                    let id = format!("call_{}", &uuid::Uuid::new_v4().to_string().replace("-", "")[..8]);

                                                    let _ = tx.send(InferenceStreamChunk {
                                                        content: vec![ContentBlock::ToolCall {
                                                            id,
                                                            name,
                                                            args: args.to_string(),
                                                        }],
                                                        is_final: false,
                                                        usage: None,
                                                        model: model_name.clone(),
                                                    }).await;
                                                }
                                            }
                                        }
                                    }

                                    // Check finish reason
                                    let finish_reason = candidate.get("finishReason")
                                        .and_then(|r| r.as_str())
                                        .unwrap_or("");

                                    if finish_reason == "STOP" || finish_reason == "stop" {
                                        // Final chunk — send usage and is_final
                                        let output_tokens = json.get("usageMetadata")
                                            .and_then(|u| u.get("candidatesTokenCount"))
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0) as u32;

                                        let usage = TokenUsage {
                                            prompt_tokens: prompt_tokens_from_start,
                                            completion_tokens: output_tokens,
                                            total_tokens: prompt_tokens_from_start + output_tokens,
                                        };

                                        let _ = tx.send(InferenceStreamChunk {
                                            content: vec![],
                                            is_final: true,
                                            usage: Some(usage),
                                            model: model_name.clone(),
                                        }).await;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Gemini SSE stream error: {:?}", e);
                        break;
                    }
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn capabilities(&self) -> ModelCapability {
        // Gemini 2.0 capabilities
        ModelCapability {
            supports_multimodal: true,
            supports_streaming: true,
            supports_tools: true,
            context_window_size: 1_000_000, // Gemini has a 1M token context window
            max_output_tokens: 8192,
        }
    }

    fn config(&self) -> &ModelConfig {
        &self.config
    }
}
