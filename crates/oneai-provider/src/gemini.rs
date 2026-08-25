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
use oneai_core::error::OneAIError;
use oneai_core::traits::LlmProvider;
use oneai_core::{
    ContentBlock, InferenceRequest, InferenceResponse, InferenceStreamChunk, Message,
    ModelCapability, ModelConfig, Role, TokenUsage,
};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use tokio_stream::wrappers::ReceiverStream;

use crate::retry::{is_retryable_status, send_with_retry, ProviderRetryConfig};

/// Google Gemini LLM provider.
///
/// Includes automatic retry on transient API errors (429 rate limits,
/// 503 service unavailable, 529 site overloaded) with exponential backoff.
pub struct GeminiProvider {
    config: ModelConfig,
    client: Client,
    /// Resolved compatibility profile (drives dispatch; see `compat.rs`).
    compat: crate::compat::Compat,
    /// Retry config for transient HTTP errors (429/503/529) + network errors.
    pub retry_config: ProviderRetryConfig,
}

impl GeminiProvider {
    /// Create a new Gemini provider with the given configuration.
    pub fn new(config: ModelConfig) -> Self {
        let client = Client::new();
        let compat = crate::compat::Compat::from_config(&config);
        Self {
            config,
            client,
            compat,
            retry_config: ProviderRetryConfig::default(),
        }
    }

    /// Create with a custom HTTP client.
    pub fn with_client(config: ModelConfig, client: Client) -> Self {
        let compat = crate::compat::Compat::from_config(&config);
        Self {
            config,
            client,
            compat,
            retry_config: ProviderRetryConfig::default(),
        }
    }

    /// Create with an explicit pre-resolved compatibility profile (factory path).
    pub(crate) fn with_compat(config: ModelConfig, compat: crate::compat::Compat) -> Self {
        let client = Client::new();
        Self {
            config,
            client,
            compat,
            retry_config: ProviderRetryConfig::default(),
        }
    }

    /// Create with custom retry configuration.
    pub fn with_retry_config(config: ModelConfig, retry_config: ProviderRetryConfig) -> Self {
        let client = Client::new();
        let compat = crate::compat::Compat::from_config(&config);
        Self {
            config,
            client,
            compat,
            retry_config,
        }
    }

    /// The resolved compatibility profile.
    pub fn compat(&self) -> crate::compat::Compat {
        self.compat
    }

    /// Set the retry configuration (builder pattern).
    pub fn retry_config(mut self, config: ProviderRetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Get the Gemini generateContent endpoint URL.
    fn generate_url(&self) -> String {
        let api_key = self.config.api_key.as_deref().unwrap_or("");
        let model = self
            .config
            .model_name
            .as_deref()
            .unwrap_or("gemini-2.0-flash");

        // Check for Vertex AI mode
        if self.config.extra.get("api_mode").map(|s| s.as_str()) == Some("vertex_ai") {
            let region = self
                .config
                .extra
                .get("region")
                .map(|s| s.as_str())
                .unwrap_or("us-central1");
            let project = self
                .config
                .extra
                .get("project")
                .cloned()
                .unwrap_or_else(|| "default".to_string());
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
        let model = self
            .config
            .model_name
            .as_deref()
            .unwrap_or("gemini-2.0-flash");

        if self.config.extra.get("api_mode").map(|s| s.as_str()) == Some("vertex_ai") {
            let region = self
                .config
                .extra
                .get("region")
                .map(|s| s.as_str())
                .unwrap_or("us-central1");
            let project = self
                .config
                .extra
                .get("project")
                .cloned()
                .unwrap_or_else(|| "default".to_string());
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
                serde_json::Number::from_f64(temperature as f64)
                    .unwrap_or(serde_json::Number::from(1)),
            );
        }
        if let Some(top_p) = req.top_p {
            generation_config["topP"] = Value::Number(
                serde_json::Number::from_f64(top_p as f64).unwrap_or(serde_json::Number::from(1)),
            );
        }
        if !req.stop_sequences.is_empty() {
            generation_config["stopSequences"] = Value::Array(
                req.stop_sequences
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            );
        }
        body["generationConfig"] = generation_config;

        // Add tool declarations (Gemini format)
        if !req.tools.is_empty() {
            let declarations: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters_schema,
                    })
                })
                .collect();

            body["tools"] = serde_json::json!([
                {
                    "function_declarations": declarations,
                }
            ]);
        }

        body
    }

    /// Parse a Gemini generateContent response.
    fn parse_response(
        json: &Value,
        model: &str,
    ) -> std::result::Result<InferenceResponse, OneAIError> {
        let candidates = json.get("candidates").and_then(|c| c.as_array());

        let mut content_blocks = Vec::new();
        let mut finish_reason = "stop".to_string();

        if let Some(candidates) = candidates {
            if let Some(candidate) = candidates.first() {
                finish_reason = candidate
                    .get("finishReason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("stop")
                    .to_string();

                let content = candidate.get("content");
                if let Some(content) = content {
                    let parts = content.get("parts").and_then(|p| p.as_array());
                    if let Some(parts) = parts {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                content_blocks.push(ContentBlock::Text {
                                    text: text.to_string(),
                                });
                            }
                            if let Some(fc) = part.get("functionCall") {
                                let name = fc
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let args = fc
                                    .get("args")
                                    .cloned()
                                    .unwrap_or(Value::Object(serde_json::Map::new()));
                                // Gemini doesn't provide call IDs — generate one
                                let id = format!(
                                    "call_{}",
                                    &uuid::Uuid::new_v4().to_string().replace("-", "")[..8]
                                );
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

        let usage = json
            .get("usageMetadata")
            .map(|u| TokenUsage {
                prompt_tokens: u
                    .get("promptTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                completion_tokens: u
                    .get("candidatesTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                total_tokens: u
                    .get("totalTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                ..Default::default()
            })
            .unwrap_or(TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                ..Default::default()
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
    async fn infer(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<InferenceResponse, OneAIError> {
        let body = self.to_gemini_request(&req);
        let url = self.generate_url();
        let model = self
            .config
            .model_name
            .as_deref()
            .unwrap_or("gemini-2.0-flash")
            .to_string();

        let response = send_with_retry(&self.retry_config, || {
            let url = url.clone();
            let body = body.clone();
            self.client
                .post(&url)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
        })
        .await
        .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| OneAIError::Network(e.to_string()))?;
            tracing::error!("Gemini API error {}: {}", status, text);
            if is_retryable_status(status) {
                return Err(OneAIError::RateLimit(format!(
                    "Gemini API rate limit error after {} retries: {} — {}",
                    self.retry_config.max_retries, status, text
                )));
            }
            return Err(OneAIError::Provider(format!(
                "Gemini API error {}: {}",
                status, text
            )));
        }

        let json: Value = response
            .json()
            .await
            .map_err(|e| OneAIError::Network(e.to_string()))?;
        Self::parse_response(&json, &model)
    }

    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError>
    {
        let body = self.to_gemini_request(&req);
        let url = self.stream_url();
        let model_name = self.config.model_name.clone();

        let response = send_with_retry(&self.retry_config, || {
            let url = url.clone();
            let body = body.clone();
            self.client
                .post(&url)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
        })
        .await
        .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| OneAIError::Network(e.to_string()))?;
            tracing::error!("Gemini API error {}: {}", status, text);
            if is_retryable_status(status) {
                return Err(OneAIError::RateLimit(format!(
                    "Gemini API rate limit error after {} retries: {} — {}",
                    self.retry_config.max_retries, status, text
                )));
            }
            return Err(OneAIError::Provider(format!(
                "Gemini API error {}: {}",
                status, text
            )));
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
                                prompt_tokens_from_start = usage
                                    .get("promptTokenCount")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as u32;
                            }

                            // Parse candidates
                            let candidates = json.get("candidates").and_then(|c| c.as_array());

                            if let Some(candidates) = candidates {
                                for candidate in candidates {
                                    let content = candidate.get("content");
                                    if let Some(content) = content {
                                        let parts = content.get("parts").and_then(|p| p.as_array());
                                        if let Some(parts) = parts {
                                            for part in parts {
                                                if let Some(text) =
                                                    part.get("text").and_then(|t| t.as_str())
                                                {
                                                    if !text.is_empty() {
                                                        let _ = tx
                                                            .send(InferenceStreamChunk {
                                                                content: vec![ContentBlock::Text {
                                                                    text: text.to_string(),
                                                                }],
                                                                is_final: false,
                                                                usage: None,
                                                                model: model_name.clone(),
                                                            })
                                                            .await;
                                                    }
                                                }
                                                if let Some(fc) = part.get("functionCall") {
                                                    let name = fc
                                                        .get("name")
                                                        .and_then(|n| n.as_str())
                                                        .unwrap_or("")
                                                        .to_string();
                                                    let args = fc.get("args").cloned().unwrap_or(
                                                        Value::Object(serde_json::Map::new()),
                                                    );
                                                    let id = format!(
                                                        "call_{}",
                                                        &uuid::Uuid::new_v4()
                                                            .to_string()
                                                            .replace("-", "")[..8]
                                                    );

                                                    let _ = tx
                                                        .send(InferenceStreamChunk {
                                                            content: vec![ContentBlock::ToolCall {
                                                                id,
                                                                name,
                                                                args: args.to_string(),
                                                            }],
                                                            is_final: false,
                                                            usage: None,
                                                            model: model_name.clone(),
                                                        })
                                                        .await;
                                                }
                                            }
                                        }
                                    }

                                    // Check finish reason
                                    let finish_reason = candidate
                                        .get("finishReason")
                                        .and_then(|r| r.as_str())
                                        .unwrap_or("");

                                    if finish_reason == "STOP" || finish_reason == "stop" {
                                        // Final chunk — send usage and is_final
                                        let output_tokens = json
                                            .get("usageMetadata")
                                            .and_then(|u| u.get("candidatesTokenCount"))
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as u32;

                                        let usage = TokenUsage {
                                            prompt_tokens: prompt_tokens_from_start,
                                            completion_tokens: output_tokens,
                                            total_tokens: prompt_tokens_from_start + output_tokens,
                                            ..Default::default()
                                        };

                                        let _ = tx
                                            .send(InferenceStreamChunk {
                                                content: vec![],
                                                is_final: true,
                                                usage: Some(usage),
                                                model: model_name.clone(),
                                            })
                                            .await;
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
        // Prefer the generated catalog's real per-model values; fall back to
        // Gemini 2.0 defaults when the model isn't catalogued.
        if let Some(cap) = self
            .config
            .model_name
            .as_deref()
            .and_then(oneai_core::catalog::capability_snapshot)
        {
            return cap;
        }
        // Gemini 2.0 capabilities
        ModelCapability {
            supports_multimodal: true,
            supports_streaming: true,
            supports_tools: true,
            context_window_size: 1_000_000, // Gemini has a 1M token context window
            max_output_tokens: 8192,
        }
    }

    /// L2 probe: query Gemini's `models.get` for the model's input token limit.
    ///
    /// Endpoint: `GET {base}/models/{id}?key={api_key}`. Response includes
    /// `inputTokenLimit` (the context window). Best-effort: any failure → `None`.
    async fn probe_context_window(&self) -> Option<u32> {
        let model = self.config.model_name.as_deref()?;
        let api_key = self.config.api_key.as_deref().unwrap_or("");
        let base = self.config.resolved_url();
        let url = format!("{}/models/{}", base.trim_end_matches('/'), model);

        let resp = self
            .client
            .get(&url)
            .query(&[("key", api_key)])
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: Value = resp.json().await.ok()?;
        parse_gemini_context_window(&json)
    }

    /// List the models served by this Gemini endpoint (`GET /models`).
    ///
    /// Powers the settings UI's model dropdown (`provider/models` RPC).
    /// Best-effort: any network/auth/parse failure returns an empty list.
    async fn list_models(&self) -> Vec<String> {
        let api_key = self.config.api_key.as_deref().unwrap_or("");
        let base = self.config.resolved_url();
        let url = format!("{}/models", base.trim_end_matches('/'));

        let resp = self
            .client
            .get(&url)
            .query(&[("key", api_key), ("pageSize", "1000")])
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        let Ok(resp) = resp else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(json) = resp.json::<Value>().await else {
            return Vec::new();
        };
        parse_gemini_model_list(&json)
    }

    fn config(&self) -> &ModelConfig {
        &self.config
    }
}

/// Parse the context-window size from a Gemini `models.get` response.
///
/// Response shape: `{"name":"models/gemini-2.5-pro","inputTokenLimit":1048576,"outputTokenLimit":8192,...}`.
pub fn parse_gemini_context_window(json: &Value) -> Option<u32> {
    let n = json.get("inputTokenLimit")?.as_u64()?;
    if n > 0 {
        Some(n.min(u32::MAX as u64) as u32)
    } else {
        None
    }
}

/// Parse model ids from a Gemini `GET /models` (list) response.
///
/// Response shape: `{"models":[{"name":"models/gemini-2.5-pro","displayName":"...","supportedGenerationMethods":["generateContent",...]},...]}`.
/// Strips the `models/` name prefix (the id the generate endpoints take),
/// keeps only entries usable for chat (those listing `generateContent` in
/// `supportedGenerationMethods`; entries without the field are kept — some
/// gateways omit it), and sorts for a stable dropdown.
pub fn parse_gemini_model_list(json: &Value) -> Vec<String> {
    let Some(models) = json.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = models
        .iter()
        .filter(|m| {
            let Some(methods) = m
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)
            else {
                return true; // field absent — keep (gateway may not report it)
            };
            methods
                .iter()
                .filter_map(Value::as_str)
                .any(|s| s == "generateContent")
        })
        .filter_map(|m| m.get("name").and_then(Value::as_str))
        .map(|name| name.strip_prefix("models/").unwrap_or(name).to_string())
        .filter(|id| !id.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_gemini_context_window() {
        let resp = json!({
            "name": "models/gemini-2.5-pro",
            "inputTokenLimit": 1048576,
            "outputTokenLimit": 8192,
        });
        assert_eq!(parse_gemini_context_window(&resp), Some(1_048_576));
    }

    #[test]
    fn test_parse_gemini_model_list() {
        let resp = json!({
            "models": [
                {
                    "name": "models/gemini-2.5-pro",
                    "displayName": "Gemini 2.5 Pro",
                    "supportedGenerationMethods": ["generateContent"]
                },
                {
                    "name": "models/gemini-embedding-001",
                    "displayName": "Gemini Embedding",
                    "supportedGenerationMethods": ["embedContent"]
                },
                {
                    "name": "models/gemini-2.5-flash",
                    "displayName": "Gemini 2.5 Flash"
                }
            ]
        });
        // Embedding-only dropped; `models/` prefix stripped; no-method entry kept.
        assert_eq!(
            parse_gemini_model_list(&resp),
            vec!["gemini-2.5-flash".to_string(), "gemini-2.5-pro".to_string()]
        );
    }

    #[test]
    fn test_parse_gemini_model_list_malformed() {
        assert_eq!(parse_gemini_model_list(&json!({})), Vec::<String>::new());
        assert_eq!(
            parse_gemini_model_list(&json!({"models": "nope"})),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_parse_gemini_missing_field() {
        let resp = json!({ "name": "models/gemini-2.5-pro" });
        assert_eq!(parse_gemini_context_window(&resp), None);
    }
}
