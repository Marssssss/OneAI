//! Anthropic Claude native protocol provider implementation.
//!
//! Uses the Anthropic API for streaming and non-streaming inference.
//! Claude uses a different API format than OpenAI (no "choices" array, direct message output).
//!
//! **Automatic retry**: 429 (rate limit) and 503/529 (service unavailable) errors
//! are automatically retried with exponential backoff (default: 3 retries, 1s→2s→4s).

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

use crate::retry::{ProviderRetryConfig, is_retryable_status, send_with_retry};

/// API mode selection for Anthropic provider.
#[derive(Debug, Clone, PartialEq)]
enum ApiMode {
    /// Messages API — classic, manually managed conversation.
    Messages,
    /// Responses API — agent-oriented, server-managed conversation state.
    Responses,
}

/// Anthropic Claude LLM provider.
///
/// Includes automatic retry on transient API errors (429 rate limits,
/// 503 service unavailable, 529 site overloaded) with exponential backoff.
pub struct AnthropicProvider {
    config: ModelConfig,
    client: Client,
    /// Retry configuration for transient API errors.
    /// Default: 3 retries with exponential backoff (1s → 2s → 4s).
    pub retry_config: ProviderRetryConfig,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider with the given configuration.
    pub fn new(config: ModelConfig) -> Self {
        let client = Client::new();
        Self {
            config,
            client,
            retry_config: ProviderRetryConfig::default(),
        }
    }

    /// Create with a custom HTTP client.
    pub fn with_client(config: ModelConfig, client: Client) -> Self {
        Self {
            config,
            client,
            retry_config: ProviderRetryConfig::default(),
        }
    }

    /// Set the retry configuration (builder pattern).
    pub fn retry_config(mut self, config: ProviderRetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Get the Anthropic Messages API endpoint URL.
    fn messages_url(&self) -> String {
        let base = self.config.resolved_url();
        format!("{}/messages", base.trim_end_matches('/'))
    }

    /// Get the Anthropic Responses API endpoint URL.
    fn responses_url(&self) -> String {
        let base = self.config.resolved_url();
        format!("{}/responses", base.trim_end_matches('/'))
    }

    /// Determine which API mode to use based on configuration.
    fn api_mode(&self) -> ApiMode {
        match self.config.extra.get("api_mode").map(|s| s.as_str()) {
            Some("responses") => ApiMode::Responses,
            _ => ApiMode::Messages,
        }
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
                _ => {} // #[non_exhaustive] catch-all
            }
        }

        let mut body = serde_json::json!({
            "model": self.config.model_name.as_deref().unwrap_or("claude-sonnet-4-20250514"),
            "messages": messages,
        });

        // Use Anthropic prompt caching — mark system prompt and tools
        // with cache_control: ephemeral to avoid re-sending static context every turn.
        // Anthropic's prompt caching requires the system field to be an array of content blocks.
        if !system_text.is_empty() {
            body["system"] = serde_json::json!([
                {
                    "type": "text",
                    "text": system_text.trim(),
                    "cache_control": { "type": "ephemeral" }
                }
            ]);
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

        // Enable extended thinking if budget is provided
        if let Some(budget) = req.thinking_budget {
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
            // When thinking is enabled, max_tokens must be >= budget_tokens + output tokens
            // Increase max_tokens if it's too small
            if let Some(max_tokens) = req.max_tokens {
                if max_tokens < budget + 4096 {
                    body["max_tokens"] = Value::Number((budget + 4096).into());
                }
            } else {
                // Default max_tokens (4096) is too small for thinking; set to budget + 4096
                body["max_tokens"] = Value::Number((budget + 4096).into());
            }
        }

        // Add tool definitions (Anthropic format)
        // Mark the last tool with cache_control: ephemeral for prompt caching.
        // Anthropic recommends placing cache breakpoints at the end of the tools array
        // so that the entire tool definitions block is cached as a single prefix.
        if !req.tools.is_empty() {
            let mut tools_json: Vec<Value> = req.tools.iter().map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters_schema,
                })
            }).collect();

            // Add cache_control to the last tool definition — this creates a
            // cache breakpoint that covers the entire system + tools prefix.
            // On subsequent turns with the same system+tools prefix, Anthropic
            // reuses the cached prefix tokens, reducing cost and latency.
            if let Some(last_tool) = tools_json.last_mut() {
                if let Some(obj) = last_tool.as_object_mut() {
                    obj.insert(
                        "cache_control".to_string(),
                        serde_json::json!({ "type": "ephemeral" })
                    );
                }
            }

            body["tools"] = Value::Array(tools_json);
        }

        body
    }

    /// Convert an InferenceRequest to Anthropic Responses API format.
    ///
    /// The Responses API is designed for agent-oriented workflows:
    /// - Uses `input` (message list) instead of `messages`
    /// - Supports `previous_response_id` for server-side conversation state
    /// - Tool results are wrapped in `function_call_output` items
    /// - Returns structured output items (text, function_call, function_call_output)
    fn to_responses_request(&self, req: &InferenceRequest) -> Value {
        let mut input_items = Vec::new();

        for msg in &req.conversation.messages {
            match msg.role {
                Role::System => {
                    // System messages are handled via the `instructions` field in Responses API
                    // (not included in input items)
                }
                Role::User => {
                    // User messages become `message` input items
                    let mut content = Vec::new();
                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } => {
                                content.push(serde_json::json!({
                                    "type": "input_text",
                                    "text": text,
                                }));
                            }
                            ContentBlock::Image { mime_type, data } => {
                                use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
                                content.push(serde_json::json!({
                                    "type": "input_image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": mime_type,
                                        "data": BASE64.encode(data),
                                    }
                                }));
                            }
                            ContentBlock::ToolResult { call_id, content } => {
                                // Tool results become `function_call_output` items in Responses API
                                input_items.push(serde_json::json!({
                                    "type": "function_call_output",
                                    "call_id": call_id,
                                    "output": content,
                                }));
                            }
                            _ => {}
                        }
                    }
                    if !content.is_empty() {
                        input_items.push(serde_json::json!({
                            "type": "message",
                            "role": "user",
                            "content": content,
                        }));
                    }
                }
                Role::Assistant => {
                    // Assistant messages become `message` input items
                    let mut content = Vec::new();
                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } => {
                                content.push(serde_json::json!({
                                    "type": "output_text",
                                    "text": text,
                                }));
                            }
                            ContentBlock::ToolCall { id, name, args } => {
                                // Tool calls become `function_call` items in Responses API
                                let input = serde_json::from_str::<Value>(args)
                                    .unwrap_or(Value::Object(serde_json::Map::new()));
                                content.push(serde_json::json!({
                                    "type": "function_call",
                                    "id": id,
                                    "name": name,
                                    "input": input,
                                }));
                            }
                            ContentBlock::Thinking { text } => {
                                content.push(serde_json::json!({
                                    "type": "thinking",
                                    "thinking": text,
                                }));
                            }
                            _ => {}
                        }
                    }
                    if !content.is_empty() {
                        input_items.push(serde_json::json!({
                            "type": "message",
                            "role": "assistant",
                            "content": content,
                        }));
                    }
                }
                Role::Tool => {
                    // Tool results in Responses API are `function_call_output` items
                    for block in &msg.content {
                        if let ContentBlock::ToolResult { call_id, content } = block {
                            input_items.push(serde_json::json!({
                                "type": "function_call_output",
                                "call_id": call_id,
                                "output": content,
                            }));
                        }
                    }
                }
                _ => {} // #[non_exhaustive] catch-all
            }
        }

        // Collect system text for the `instructions` field
        let instructions = req.conversation.messages.iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");

        let mut body = serde_json::json!({
            "model": self.config.model_name.as_deref().unwrap_or("claude-sonnet-4-20250514"),
            "input": input_items,
        });

        if !instructions.is_empty() {
            body["instructions"] = Value::String(instructions.trim().to_string());
        }

        if let Some(max_tokens) = req.max_tokens {
            body["max_output_tokens"] = Value::Number(max_tokens.into());
        } else {
            body["max_output_tokens"] = Value::Number(4096.into());
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

        // Enable extended thinking if budget is provided
        if let Some(budget) = req.thinking_budget {
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
            if let Some(max_tokens) = req.max_tokens {
                if max_tokens < budget + 4096 {
                    body["max_output_tokens"] = Value::Number((budget + 4096).into());
                }
            } else {
                body["max_output_tokens"] = Value::Number((budget + 4096).into());
            }
        }

        // Add tool definitions (Responses API format)
        if !req.tools.is_empty() {
            let tools_json: Vec<Value> = req.tools.iter().map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters_schema,
                })
            }).collect();

            body["tools"] = Value::Array(tools_json);
        }

        // Add stream mode for Responses API
        body["stream"] = Value::Bool(false);

        body
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn infer(&self, req: InferenceRequest) -> std::result::Result<InferenceResponse, OneAIError> {
        match self.api_mode() {
            ApiMode::Messages => self.infer_messages(req).await,
            ApiMode::Responses => self.infer_responses(req).await,
        }
    }

    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError> {
        match self.api_mode() {
            ApiMode::Messages => self.infer_stream_messages(req).await,
            ApiMode::Responses => self.infer_stream_responses(req).await,
        }
    }

    fn capabilities(&self) -> ModelCapability {
        ModelCapability::claude_class()
    }

    /// L2 probe: query Anthropic's `/v1/models/{id}` for the model's context window.
    ///
    /// Anthropic's Models API returns `{"id":..., "context_window": 200000, ...}`.
    /// Best-effort: any network/parse/auth failure returns `None`.
    async fn probe_context_window(&self) -> Option<u32> {
        let model = self.config.model_name.as_deref()?;
        let api_key = self.config.api_key.as_deref()?;
        let base = self.config.resolved_url();
        let url = format!("{}/models/{}", base.trim_end_matches('/'), model);

        let resp = self
            .client
            .get(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: Value = resp.json().await.ok()?;
        parse_anthropic_context_window(&json)
    }

    fn config(&self) -> &ModelConfig {
        &self.config
    }
}

impl AnthropicProvider {
    /// Messages API: non-streaming inference.
    async fn infer_messages(&self, req: InferenceRequest) -> std::result::Result<InferenceResponse, OneAIError> {
        let body = self.to_anthropic_request(&req);
        let url = self.messages_url();

        let response = send_with_retry(
            &self.retry_config,
            || {
                let url = url.clone();
                let body = body.clone();
                let api_key = self.config.api_key.as_deref().unwrap_or("").to_string();
                self.client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
            },
        )
        .await
        .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            if is_retryable_status(status) {
                return Err(OneAIError::RateLimit(format!(
                    "Anthropic API rate limit error after {} retries: {} — {}",
                    self.retry_config.max_retries, status, text
                )));
            }
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
                    "thinking" => {
                        let text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                        content_blocks.push(ContentBlock::Thinking { text: text.to_string() });
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

    /// Messages API: streaming inference.
    async fn infer_stream_messages(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError> {
        let mut body = self.to_anthropic_request(&req);
        body["stream"] = Value::Bool(true);

        let url = self.messages_url();

        let response = send_with_retry(
            &self.retry_config,
            || {
                let url = url.clone();
                let body = body.clone();
                let api_key = self.config.api_key.as_deref().unwrap_or("").to_string();
                self.client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
            },
        )
        .await
        .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            if is_retryable_status(status) {
                return Err(OneAIError::RateLimit(format!(
                    "Anthropic API rate limit error after {} retries: {} — {}",
                    self.retry_config.max_retries, status, text
                )));
            }
            return Err(OneAIError::Provider(format!("Anthropic API error {}: {}", status, text)));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        let model_name = self.config.model_name.clone();
        tokio::spawn(async move {
            let stream = response.bytes_stream();
            // Track input_tokens from message_start event
            let mut prompt_tokens_from_start: u32 = 0;

            // Per-tool-call-id state: (name, args_buffer)
            // Used to accumulate input_json_delta fragments for each tool call
            let mut tool_call_state: HashMap<String, (String, String)> = HashMap::new();
            // Current tool call ID being streamed (set by content_block_start, cleared by content_block_stop)
            let mut current_tool_call_id: Option<String> = None;

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
                                "message_start" => {
                                    // Capture input_tokens from the message_start event
                                    let msg = json.get("message").unwrap_or(&Value::Null);
                                    let usage_obj = msg.get("usage").unwrap_or(&Value::Null);
                                    prompt_tokens_from_start = usage_obj.get("input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32;
                                }
                                "content_block_start" => {
                                    let content_block = json.get("content_block").unwrap_or(&Value::Null);
                                    let cb_type = content_block.get("type").and_then(|t| t.as_str()).unwrap_or("");

                                    if cb_type == "tool_use" {
                                        let id = content_block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        let name = content_block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        // Initialize args buffer for this tool call
                                        tool_call_state.insert(id.clone(), (name.clone(), String::new()));
                                        current_tool_call_id = Some(id.clone());

                                        // Emit ToolCall with id and name, empty args (intent detected)
                                        let _ = tx.send(InferenceStreamChunk {
                                            content: vec![ContentBlock::ToolCall {
                                                id: id.clone(),
                                                name,
                                                args: String::new(), // Args will be filled on content_block_stop
                                            }],
                                            is_final: false,
                                            usage: None,
                                            model: model_name.clone(),
                                        }).await;
                                    }
                                }
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
                                            // Accumulate partial JSON into the current tool call's args buffer
                                            let partial_json = delta.get("partial_json").and_then(|p| p.as_str()).unwrap_or("");
                                            if let Some(tc_id) = &current_tool_call_id {
                                                if let Some((_name, args_buffer)) = tool_call_state.get_mut(tc_id) {
                                                    args_buffer.push_str(partial_json);
                                                }
                                            }
                                        }
                                        "thinking_delta" => {
                                            let text = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                                            if !text.is_empty() {
                                                let _ = tx.send(InferenceStreamChunk {
                                                    content: vec![ContentBlock::Thinking { text: text.to_string() }],
                                                    is_final: false,
                                                    usage: None,
                                                    model: model_name.clone(),
                                                }).await;
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                "content_block_stop" => {
                                    // If we were accumulating a tool call, finalize it with complete args
                                    if let Some(tc_id) = current_tool_call_id.take() {
                                        if let Some((name, args_buffer)) = tool_call_state.remove(&tc_id) {
                                            // The args_buffer contains all accumulated partial_json fragments
                                            // Validate it's proper JSON; if not, wrap it as-is
                                            let args = if args_buffer.is_empty() {
                                                "{}".to_string()
                                            } else {
                                                // Try to parse as JSON to validate
                                                if serde_json::from_str::<Value>(&args_buffer).is_ok() {
                                                    args_buffer
                                                } else {
                                                    // If invalid JSON, still pass it (provider may send incomplete)
                                                    args_buffer
                                                }
                                            };

                                            let _ = tx.send(InferenceStreamChunk {
                                                content: vec![ContentBlock::ToolCall {
                                                    id: tc_id.clone(),
                                                    name: name.clone(),
                                                    args,
                                                }],
                                                is_final: false,
                                                usage: None,
                                                model: model_name.clone(),
                                            }).await;
                                        }
                                    }
                                }
                                "message_delta" => {
                                    let delta = json.get("delta").unwrap_or(&Value::Null);
                                    let stop_reason = delta.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("");

                                    let usage_obj = json.get("usage").unwrap_or(&Value::Null);
                                    let output_tokens = usage_obj.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                    let usage = TokenUsage {
                                        prompt_tokens: prompt_tokens_from_start,
                                        completion_tokens: output_tokens,
                                        total_tokens: prompt_tokens_from_start + output_tokens,
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

    /// Responses API: non-streaming inference.
    async fn infer_responses(&self, req: InferenceRequest) -> std::result::Result<InferenceResponse, OneAIError> {
        let body = self.to_responses_request(&req);
        let url = self.responses_url();

        let response = send_with_retry(
            &self.retry_config,
            || {
                let url = url.clone();
                let body = body.clone();
                let api_key = self.config.api_key.as_deref().unwrap_or("").to_string();
                self.client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
            },
        )
        .await
        .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            if is_retryable_status(status) {
                return Err(OneAIError::RateLimit(format!(
                    "Anthropic API rate limit error after {} retries: {} — {}",
                    self.retry_config.max_retries, status, text
                )));
            }
            return Err(OneAIError::Provider(format!("Anthropic Responses API error {}: {}", status, text)));
        }

        let json: Value = response.json().await.map_err(|e| OneAIError::Network(e.to_string()))?;

        // Parse Anthropic Responses API format
        // The response has a different structure from Messages API:
        // - `output` array contains items (message, function_call, function_call_output)
        // - `usage` is in the top-level object
        let model = json.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();

        let output_items = json.get("output").and_then(|o| o.as_array());

        let mut content_blocks = Vec::new();
        if let Some(items) = output_items {
            for item in items {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match item_type {
                    "message" => {
                        // Message items have a content array
                        let content = item.get("content").and_then(|c| c.as_array());
                        if let Some(content) = content {
                            for block in content {
                                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                match block_type {
                                    "output_text" | "text" => {
                                        let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                        content_blocks.push(ContentBlock::Text { text: text.to_string() });
                                    }
                                    "thinking" => {
                                        let text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                                        content_blocks.push(ContentBlock::Thinking { text: text.to_string() });
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    "function_call" => {
                        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let input = item.get("input").cloned().unwrap_or(Value::Object(serde_json::Map::new()));
                        let args = input.to_string();
                        content_blocks.push(ContentBlock::ToolCall { id, name, args });
                    }
                    "function_call_output" => {
                        // Tool results — these are returned in the output but
                        // they're part of the conversation history, not the current response
                        let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let output = item.get("output").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        content_blocks.push(ContentBlock::ToolResult { call_id, content: output });
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

        let stop_reason = json.get("status").and_then(|s| s.as_str()).unwrap_or("completed");

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

    /// Responses API: streaming inference.
    async fn infer_stream_responses(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError> {
        let mut body = self.to_responses_request(&req);
        body["stream"] = Value::Bool(true);

        let url = self.responses_url();

        let response = send_with_retry(
            &self.retry_config,
            || {
                let url = url.clone();
                let body = body.clone();
                let api_key = self.config.api_key.as_deref().unwrap_or("").to_string();
                self.client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
            },
        )
        .await
        .map_err(|e| OneAIError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
            if is_retryable_status(status) {
                return Err(OneAIError::RateLimit(format!(
                    "Anthropic API rate limit error after {} retries: {} — {}",
                    self.retry_config.max_retries, status, text
                )));
            }
            return Err(OneAIError::Provider(format!("Anthropic Responses API error {}: {}", status, text)));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        let model_name = self.config.model_name.clone();
        tokio::spawn(async move {
            let stream = response.bytes_stream();
            let mut prompt_tokens_from_start: u32 = 0;

            // Per-function-call-id state: (name, args_buffer)
            let mut function_call_state: HashMap<String, (String, String)> = HashMap::new();
            let mut current_function_call_id: Option<String> = None;

            use eventsource_stream::Eventsource;
            let mut sse_stream = stream.eventsource();

            while let Some(event) = sse_stream.next().await {
                match event {
                    Ok(event) => {
                        if let Ok(json) = serde_json::from_str::<Value>(&event.data) {
                            let event_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");

                            match event_type {
                                "response.start" => {
                                    let usage_obj = json.get("response")
                                        .and_then(|r| r.get("usage"))
                                        .unwrap_or(&Value::Null);
                                    prompt_tokens_from_start = usage_obj.get("input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32;
                                }
                                "response.output_item.start" => {
                                    let item = json.get("item").unwrap_or(&Value::Null);
                                    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    if item_type == "function_call" {
                                        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        function_call_state.insert(id.clone(), (name.clone(), String::new()));
                                        current_function_call_id = Some(id.clone());

                                        let _ = tx.send(InferenceStreamChunk {
                                            content: vec![ContentBlock::ToolCall {
                                                id: id.clone(),
                                                name,
                                                args: String::new(),
                                            }],
                                            is_final: false,
                                            usage: None,
                                            model: model_name.clone(),
                                        }).await;
                                    }
                                }
                                "response.output_text.delta" => {
                                    let text = json.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                                    if !text.is_empty() {
                                        let _ = tx.send(InferenceStreamChunk {
                                            content: vec![ContentBlock::Text { text: text.to_string() }],
                                            is_final: false,
                                            usage: None,
                                            model: model_name.clone(),
                                        }).await;
                                    }
                                }
                                "response.function_call_arguments.delta" => {
                                    let partial = json.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                                    if let Some(fc_id) = &current_function_call_id {
                                        if let Some((_name, args_buffer)) = function_call_state.get_mut(fc_id) {
                                            args_buffer.push_str(partial);
                                        }
                                    }
                                }
                                "response.output_item.done" => {
                                    if let Some(fc_id) = current_function_call_id.take() {
                                        if let Some((name, args_buffer)) = function_call_state.remove(&fc_id) {
                                            let args = if args_buffer.is_empty() {
                                                "{}".to_string()
                                            } else {
                                                args_buffer
                                            };
                                            let _ = tx.send(InferenceStreamChunk {
                                                content: vec![ContentBlock::ToolCall {
                                                    id: fc_id.clone(),
                                                    name: name.clone(),
                                                    args,
                                                }],
                                                is_final: false,
                                                usage: None,
                                                model: model_name.clone(),
                                            }).await;
                                        }
                                    }
                                }
                                "response.thinking.delta" => {
                                    let text = json.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                                    if !text.is_empty() {
                                        let _ = tx.send(InferenceStreamChunk {
                                            content: vec![ContentBlock::Thinking { text: text.to_string() }],
                                            is_final: false,
                                            usage: None,
                                            model: model_name.clone(),
                                        }).await;
                                    }
                                }
                                "response.done" => {
                                    let usage_obj = json.get("response")
                                        .and_then(|r| r.get("usage"))
                                        .unwrap_or(&Value::Null);
                                    let output_tokens = usage_obj.get("output_tokens")
                                        .and_then(|v| v.as_u64()).unwrap_or(0) as u32;
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
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Anthropic Responses SSE stream error: {:?}", e);
                        break;
                    }
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}
// ─── Anthropic /v1/models context-window parser ──────────────────────────────

/// Parse the context-window size from an Anthropic `/v1/models/{id}` response.
///
/// Response shape: `{"id":"claude-opus-4-8","display_name":"...","context_window":200000}`.
pub fn parse_anthropic_context_window(json: &Value) -> Option<u32> {
    let n = json.get("context_window")?.as_u64()?;
    if n > 0 {
        Some(n.min(u32::MAX as u64) as u32)
    } else {
        None
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_anthropic_context_window() {
        let resp = json!({
            "id": "claude-opus-4-8",
            "display_name": "Claude Opus 4.8",
            "context_window": 200000,
        });
        assert_eq!(parse_anthropic_context_window(&resp), Some(200_000));
    }

    #[test]
    fn test_parse_anthropic_missing_field() {
        let resp = json!({ "id": "claude-opus-4-8" });
        assert_eq!(parse_anthropic_context_window(&resp), None);
    }
}
