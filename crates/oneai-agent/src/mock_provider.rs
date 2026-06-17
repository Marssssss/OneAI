//! MockProvider — deterministic LLM provider for E2E testing.
//!
//! Returns scripted responses in sequence, allowing tests to verify
//! the AgentLoop's full execution path without needing a real LLM.
//!
//! Key features:
//! - `infer()` — returns the next scripted response from the queue
//! - `infer_stream()` — splits a response into multiple stream chunks
//! - Script exhaustion — returns a default DirectAnswer when all scripts are consumed
//! - Call tracking — records all inference calls for test assertions
//!
//! Usage:
//! ```ignore
//! let provider = MockProvider::from_script(vec![
//!     ScriptedResponse::tool_call("read_file", json!({"path": "/test.txt"})),
//!     ScriptedResponse::direct_answer("The file contains: hello world"),
//! ]);
//! let agent_loop = AgentLoop::new(
//!     Arc::new(provider), tools, parser, approval_gate, ...
//! );
//! let result = agent_loop.run("Read /test.txt").await?;
//! assert_eq!(result.iterations, 2);
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::pin::Pin;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

use oneai_core::{
    ContentBlock, InferenceRequest, InferenceResponse, InferenceStreamChunk,
    Message, ModelCapability, ModelConfig, Role, TokenUsage,
};
use oneai_core::error::OneAIError;
use oneai_core::traits::LlmProvider;

// ─── ScriptedResponse ────────────────────────────────────────────────────────

/// A pre-scripted response that MockProvider will return on the next infer() call.
///
/// Convenience constructors handle the common patterns:
/// - `direct_answer()` — text-only response (loop ends)
/// - `tool_call()` — one or more tool call requests
/// - `delegate()` — delegate to a sub-agent
/// - `switch_paradigm()` — switch to a different paradigm
/// - `thinking_then_answer()` — thinking content followed by a text answer
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    /// The content blocks for this response.
    pub content: Vec<ContentBlock>,
    /// Simulated token usage.
    pub usage: TokenUsage,
    /// Simulated model name.
    pub model: String,
}

impl ScriptedResponse {
    /// Create a DirectAnswer response — the model produces a final text answer.
    ///
    /// This causes the AgentLoop to terminate after one iteration.
    pub fn direct_answer(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
            },
            model: "mock-model".to_string(),
        }
    }

    /// Create a single tool call response — the model wants to invoke one tool.
    ///
    /// The AgentLoop will execute the tool and continue to the next iteration.
    pub fn tool_call(tool_name: &str, args: serde_json::Value) -> Self {
        let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        Self {
            content: vec![ContentBlock::ToolCall {
                id: format!("call_{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
                name: tool_name.to_string(),
                args: args_str,
            }],
            usage: TokenUsage {
                prompt_tokens: 200,
                completion_tokens: 30,
                total_tokens: 230,
            },
            model: "mock-model".to_string(),
        }
    }

    /// Create a response with multiple tool calls in parallel.
    pub fn tool_calls(calls: Vec<(String, serde_json::Value)>) -> Self {
        let content: Vec<ContentBlock> = calls.iter().map(|(name, args)| {
            let args_str = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
            ContentBlock::ToolCall {
                id: format!("call_{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
                name: name.clone(),
                args: args_str,
            }
        }).collect();

        Self {
            content,
            usage: TokenUsage {
                prompt_tokens: 200,
                completion_tokens: 60,
                total_tokens: 260,
            },
            model: "mock-model".to_string(),
        }
    }

    /// Create a Delegate response — the model wants to delegate a subtask.
    ///
    /// This triggers SubAgentFactory::create() and SubAgent::run().
    pub fn delegate(task: &str, agent_type: &str, budget_tokens: u32) -> Self {
        Self {
            content: vec![ContentBlock::ToolCall {
                id: format!("call_{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
                name: "delegate".to_string(),
                args: serde_json::to_string(&serde_json::json!({
                    "task": task,
                    "agent_type": agent_type,
                    "budget_tokens": budget_tokens,
                })).unwrap_or_else(|_| "{}".to_string()),
            }],
            usage: TokenUsage {
                prompt_tokens: 150,
                completion_tokens: 40,
                total_tokens: 190,
            },
            model: "mock-model".to_string(),
        }
    }

    /// Create a SwitchParadigm response — the model wants to change paradigm.
    pub fn switch_paradigm(paradigm: &str) -> Self {
        Self {
            content: vec![ContentBlock::ToolCall {
                id: format!("call_{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
                name: "switch_paradigm".to_string(),
                args: serde_json::to_string(&serde_json::json!({
                    "paradigm": paradigm,
                })).unwrap_or_else(|_| "{}".to_string()),
            }],
            usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
            },
            model: "mock-model".to_string(),
        }
    }

    /// Create a response with thinking content followed by a text answer.
    ///
    /// Simulates extended thinking models (Anthropic, DeepSeek).
    pub fn thinking_then_answer(thinking: &str, answer: &str) -> Self {
        Self {
            content: vec![
                ContentBlock::Thinking { text: thinking.to_string() },
                ContentBlock::Text { text: answer.to_string() },
            ],
            usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 80, // thinking + answer
                total_tokens: 180,
            },
            model: "mock-model".to_string(),
        }
    }

    /// Create a response with thinking content followed by a tool call.
    pub fn thinking_then_tool_call(thinking: &str, tool_name: &str, args: serde_json::Value) -> Self {
        let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        Self {
            content: vec![
                ContentBlock::Thinking { text: thinking.to_string() },
                ContentBlock::ToolCall {
                    id: format!("call_{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
                    name: tool_name.to_string(),
                    args: args_str,
                },
            ],
            usage: TokenUsage {
                prompt_tokens: 150,
                completion_tokens: 60,
                total_tokens: 210,
            },
            model: "mock-model".to_string(),
        }
    }

    /// Create a custom response with arbitrary content blocks.
    pub fn custom(content: Vec<ContentBlock>, usage: TokenUsage) -> Self {
        Self {
            content,
            usage,
            model: "mock-model".to_string(),
        }
    }

    /// Create a text + tool call combined response.
    /// The model produces both text commentary and a tool call.
    pub fn text_and_tool_call(text: &str, tool_name: &str, args: serde_json::Value) -> Self {
        let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        Self {
            content: vec![
                ContentBlock::Text { text: text.to_string() },
                ContentBlock::ToolCall {
                    id: format!("call_{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
                    name: tool_name.to_string(),
                    args: args_str,
                },
            ],
            usage: TokenUsage {
                prompt_tokens: 200,
                completion_tokens: 80,
                total_tokens: 280,
            },
            model: "mock-model".to_string(),
        }
    }
}

// ─── InferenceCallLog ────────────────────────────────────────────────────────

/// Record of an inference call — for test assertions.
#[derive(Debug, Clone)]
pub struct InferenceCallLog {
    /// The request that was sent.
    pub request: InferenceRequest,
    /// The index in the script that was used.
    pub script_index: usize,
}

// ─── MockProvider ──────────────────────────────────────────────────────────────

/// A deterministic mock LLM provider that returns scripted responses.
///
/// Each call to `infer()` advances the script index and returns the next
/// scripted response. When all scripts are consumed, a default DirectAnswer
/// is returned ("I have completed the task.").
///
/// Call tracking: all inference calls are recorded in `call_log` for
/// test assertions. Use `mock_provider.call_log()` to verify what
/// requests were sent and which scripts were used.
///
/// Streaming: `infer_stream()` splits the scripted response into
/// individual ContentBlock chunks, simulating real streaming behavior.
pub struct MockProvider {
    /// Scripted responses queue.
    responses: Vec<ScriptedResponse>,
    /// Current script index (advances on each infer() call).
    current_index: Arc<Mutex<usize>>,
    /// Model configuration.
    config: ModelConfig,
    /// Call log — records all inference calls.
    call_log: Arc<Mutex<Vec<InferenceCallLog>>>,
}

impl MockProvider {
    /// Create a MockProvider from a scripted response sequence.
    pub fn from_script(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            responses,
            current_index: Arc::new(Mutex::new(0)),
            config: ModelConfig::openai("mock-key".to_string(), "mock-model".to_string()),
            call_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Create a MockProvider that always returns a DirectAnswer.
    /// Useful for simple tests where you just want the loop to terminate immediately.
    pub fn always_answers(text: impl Into<String>) -> Self {
        Self::from_script(vec![ScriptedResponse::direct_answer(text)])
    }

    /// Create a MockProvider with a custom ModelConfig.
    pub fn with_config(responses: Vec<ScriptedResponse>, config: ModelConfig) -> Self {
        Self {
            responses,
            current_index: Arc::new(Mutex::new(0)),
            config,
            call_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Get all recorded inference calls.
    pub async fn call_log(&self) -> Vec<InferenceCallLog> {
        self.call_log.lock().await.clone()
    }

    /// Get the number of inference calls made.
    pub async fn call_count(&self) -> usize {
        self.call_log.lock().await.len()
    }

    /// Get the current script index (how many scripts have been consumed).
    pub async fn current_index(&self) -> usize {
        *self.current_index.lock().await
    }

    /// Check if all scripts have been consumed.
    pub async fn is_exhausted(&self) -> bool {
        *self.current_index.lock().await >= self.responses.len()
    }

    /// Get the next scripted response, or the default exhaustion response.
    fn get_next_response(&self, index: usize) -> InferenceResponse {
        if index < self.responses.len() {
            let script = &self.responses[index];
            InferenceResponse {
                message: Message {
                    role: Role::Assistant,
                    content: script.content.clone(),
                    metadata: HashMap::new(),
                },
                usage: script.usage.clone(),
                model: script.model.clone(),
                metadata: HashMap::new(),
            }
        } else {
            // Script exhausted — return a default DirectAnswer
            InferenceResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "I have completed the task.".to_string(),
                    }],
                    metadata: HashMap::new(),
                },
                usage: TokenUsage {
                    prompt_tokens: 50,
                    completion_tokens: 20,
                    total_tokens: 70,
                },
                model: "mock-model".to_string(),
                metadata: HashMap::new(),
            }
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn infer(&self, req: InferenceRequest) -> std::result::Result<InferenceResponse, OneAIError> {
        let mut index = self.current_index.lock().await;

        // Log this call
        self.call_log.lock().await.push(InferenceCallLog {
            request: req,
            script_index: *index,
        });

        let response = self.get_next_response(*index);
        *index += 1;

        Ok(response)
    }

    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError> {
        let mut index = self.current_index.lock().await;

        // Log this call
        self.call_log.lock().await.push(InferenceCallLog {
            request: req,
            script_index: *index,
        });

        // Get the scripted response, then split it into stream chunks
        let response = self.get_next_response(*index);
        *index += 1;

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        // Spawn a task that emits each content block as incremental stream chunks.
        // This addresses the MockProvider streaming compatibility gap:
        // the IncrementalStreamParser expects incremental fragments for ToolCall blocks
        // (name intent chunk + arg continuation chunks), not complete blocks in one chunk.
        //
        // For ToolCall blocks, we emit:
        //   1. Name intent chunk: ContentBlock::ToolCall { id, name, args: "" }
        //   2. Arg continuation chunks: ContentBlock::ToolCall { id: "", name: "", args: fragment }
        //      (args are split into ~20-char fragments)
        //
        // For Thinking blocks, we emit fragments (compatible with the ThinkingFragment fix).
        //
        // For Text blocks, we emit the complete text in one chunk (text fragments are
        // displayed immediately, no incremental benefit).
        tokio::spawn(async move {
            for block in &response.message.content {
                match block {
                    ContentBlock::ToolCall { id, name, args } => {
                        // Phase 1: emit name intent (tool call detected)
                        tx.send(InferenceStreamChunk {
                            content: vec![ContentBlock::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                args: String::new(), // Empty args — intent only
                            }],
                            is_final: false,
                            usage: None,
                            model: Some(response.model.clone()),
                        }).await.ok();

                        // Phase 2: emit arg fragments (~20 chars each)
                        if !args.is_empty() {
                            let chunk_size = 20;
                            let chars = args.chars().collect::<Vec<_>>();
                            for start in (0..chars.len()).step_by(chunk_size) {
                                let end = std::cmp::min(start + chunk_size, chars.len());
                                let fragment: String = chars[start..end].iter().collect();
                                tx.send(InferenceStreamChunk {
                                    content: vec![ContentBlock::ToolCall {
                                        id: String::new(),    // Empty id = arg continuation
                                        name: String::new(),  // Empty name = arg continuation
                                        args: fragment,
                                    }],
                                    is_final: false,
                                    usage: None,
                                    model: Some(response.model.clone()),
                                }).await.ok();
                            }
                        }
                    }
                    ContentBlock::Thinking { text } => {
                        // Emit thinking fragments (~30 chars each) for incremental display
                        if !text.is_empty() {
                            let chunk_size = 30;
                            let chars = text.chars().collect::<Vec<_>>();
                            for start in (0..chars.len()).step_by(chunk_size) {
                                let end = std::cmp::min(start + chunk_size, chars.len());
                                let fragment: String = chars[start..end].iter().collect();
                                tx.send(InferenceStreamChunk {
                                    content: vec![ContentBlock::Thinking {
                                        text: fragment,
                                    }],
                                    is_final: false,
                                    usage: None,
                                    model: Some(response.model.clone()),
                                }).await.ok();
                            }
                        }
                    }
                    // Text blocks are emitted as-is (immediate display, no incremental benefit)
                    _ => {
                        tx.send(InferenceStreamChunk {
                            content: vec![block.clone()],
                            is_final: false,
                            usage: None,
                            model: Some(response.model.clone()),
                        }).await.ok();
                    }
                }
            }

            // Final chunk with usage info
            tx.send(InferenceStreamChunk {
                content: vec![],
                is_final: true,
                usage: Some(response.usage.clone()),
                model: Some(response.model.clone()),
            }).await.ok();
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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_provider_direct_answer() {
        let provider = MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("The answer is 42"),
        ]);

        let req = InferenceRequest {
            conversation: oneai_core::Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let response = provider.infer(req).await.unwrap();
        assert_eq!(response.message.content.len(), 1);
        assert_eq!(
            response.message.content[0],
            ContentBlock::Text { text: "The answer is 42".to_string() }
        );
        assert_eq!(provider.call_count().await, 1);
    }

    #[tokio::test]
    async fn test_mock_provider_tool_call() {
        let provider = MockProvider::from_script(vec![
            ScriptedResponse::tool_call("read_file", serde_json::json!({"path": "/test.txt"})),
            ScriptedResponse::direct_answer("The file says hello"),
        ]);

        let req = InferenceRequest {
            conversation: oneai_core::Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        // First call — should return tool call
        let response1 = provider.infer(req.clone()).await.unwrap();
        assert!(matches!(
            &response1.message.content[0],
            ContentBlock::ToolCall { name, .. } if name == "read_file"
        ));

        // Second call — should return direct answer
        let response2 = provider.infer(req).await.unwrap();
        assert_eq!(
            response2.message.content[0],
            ContentBlock::Text { text: "The file says hello".to_string() }
        );

        assert_eq!(provider.call_count().await, 2);
    }

    #[tokio::test]
    async fn test_mock_provider_script_exhaustion() {
        let provider = MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("First answer"),
        ]);

        let req = InferenceRequest {
            conversation: oneai_core::Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        // First call — returns scripted response
        let response1 = provider.infer(req.clone()).await.unwrap();
        assert_eq!(
            response1.message.content[0],
            ContentBlock::Text { text: "First answer".to_string() }
        );

        // Second call — script exhausted, returns default
        let response2 = provider.infer(req).await.unwrap();
        assert_eq!(
            response2.message.content[0],
            ContentBlock::Text { text: "I have completed the task.".to_string() }
        );

        assert!(provider.is_exhausted().await);
    }

    #[tokio::test]
    async fn test_mock_provider_streaming() {
        let provider = MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("Hello world"),
        ]);

        let req = InferenceRequest {
            conversation: oneai_core::Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let stream = provider.infer_stream(req).await.unwrap();

        let chunks: Vec<InferenceStreamChunk> = stream.collect().await;

        // Should have 2 chunks: 1 content + 1 final
        assert_eq!(chunks.len(), 2);
        assert!(!chunks[0].is_final);
        assert!(chunks[1].is_final);
        assert!(chunks[1].usage.is_some());
    }

    #[tokio::test]
    async fn test_mock_provider_delegate() {
        let provider = MockProvider::from_script(vec![
            ScriptedResponse::delegate("search for bugs", "Explore", 5000),
        ]);

        let req = InferenceRequest {
            conversation: oneai_core::Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let response = provider.infer(req).await.unwrap();
        let block = &response.message.content[0];
        assert!(matches!(block, ContentBlock::ToolCall { name, .. } if name == "delegate"));
    }

    #[tokio::test]
    async fn test_mock_provider_switch_paradigm() {
        let provider = MockProvider::from_script(vec![
            ScriptedResponse::switch_paradigm("plan"),
        ]);

        let req = InferenceRequest {
            conversation: oneai_core::Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let response = provider.infer(req).await.unwrap();
        let block = &response.message.content[0];
        assert!(matches!(block, ContentBlock::ToolCall { name, .. } if name == "switch_paradigm"));
    }

    #[tokio::test]
    async fn test_mock_provider_call_log() {
        let provider = MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("answer 1"),
            ScriptedResponse::direct_answer("answer 2"),
        ]);

        let req = InferenceRequest {
            conversation: oneai_core::Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        provider.infer(req.clone()).await.unwrap();
        provider.infer(req).await.unwrap();

        let log = provider.call_log().await;
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].script_index, 0);
        assert_eq!(log[1].script_index, 1);
    }

    #[tokio::test]
    async fn test_mock_provider_thinking_then_answer() {
        let provider = MockProvider::from_script(vec![
            ScriptedResponse::thinking_then_answer(
                "Let me think about this...",
                "The answer is 42"
            ),
        ]);

        let req = InferenceRequest {
            conversation: oneai_core::Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let response = provider.infer(req).await.unwrap();
        assert_eq!(response.message.content.len(), 2);
        assert!(matches!(
            &response.message.content[0],
            ContentBlock::Thinking { text } if text == "Let me think about this..."
        ));
        assert!(matches!(
            &response.message.content[1],
            ContentBlock::Text { text } if text == "The answer is 42"
        ));
    }
}
