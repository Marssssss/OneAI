//! Core data types for the OneAI framework.
//!
//! This module defines the fundamental types that flow through the entire framework:
//! `ContentBlock`, `Message`, `Conversation`, `ModelConfig`, `ModelCapability`,
//! inference request/response types, and various supporting enums and structs.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ─── ContentBlock ────────────────────────────────────────────────────────────

/// Sealed content block type — the universal unit of multimodal content.
///
/// Models the sealed class hierarchy from the design specification:
/// Text, Image, File, ToolCall, ToolResult.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// Plain text content.
    #[serde(rename = "text")]
    Text {
        text: String,
    },

    /// Image content with raw bytes.
    #[serde(rename = "image")]
    Image {
        mime_type: String,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },

    /// File reference by URI.
    #[serde(rename = "file")]
    File {
        mime_type: String,
        uri: String,
    },

    /// A tool call request from the model.
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        args: String, // JSON string of arguments
    },

    /// The result of a tool call, returned to the model.
    #[serde(rename = "tool_result")]
    ToolResult {
        call_id: String,
        content: String,
    },

    /// Thinking/reasoning content from extended thinking models (Anthropic, DeepSeek).
    #[serde(rename = "thinking")]
    Thinking {
        text: String,
    },
}

/// Base64 serialization helpers for byte arrays in ContentBlock::Image.
mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &Vec<u8>, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&BASE64.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        BASE64.decode(&s).map_err(serde::de::Error::custom)
    }
}

// ─── Role ─────────────────────────────────────────────────────────────────────

/// Message role in a conversation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

// ─── Message ──────────────────────────────────────────────────────────────────

/// A single message in a conversation, containing one or more content blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    /// The role of the message author.
    pub role: Role,

    /// Content blocks — supports multimodal (text + images + files + tool calls).
    pub content: Vec<ContentBlock>,

    /// Optional metadata (timestamps, source info, etc.).
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl Message {
    /// Create a simple text message.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            metadata: HashMap::new(),
        }
    }

    /// Create a system message.
    pub fn system(text: impl Into<String>) -> Self {
        Self::text(Role::System, text)
    }

    /// Create a user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self::text(Role::User, text)
    }

    /// Create an assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::text(Role::Assistant, text)
    }

    /// Create a tool result message.
    pub fn tool_result(call_id: String, content: String) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult { call_id, content }],
            metadata: HashMap::new(),
        }
    }

    /// Extract all text content from this message.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Extract all tool calls from this message.
    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
            .collect()
    }
}

// ─── Conversation ─────────────────────────────────────────────────────────────

/// A conversation consisting of a sequence of messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conversation {
    /// Unique conversation identifier.
    pub id: String,

    /// The messages in this conversation.
    pub messages: Vec<Message>,

    /// Optional metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl Conversation {
    /// Create a new empty conversation.
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            messages: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Create a conversation with a given ID.
    pub fn with_id(id: String) -> Self {
        Self {
            id,
            messages: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Add a message to the conversation.
    pub fn add_message(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Get the last message in the conversation.
    pub fn last_message(&self) -> Option<&Message> {
        self.messages.last()
    }

    /// Count the number of messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Check if the conversation is empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

impl Default for Conversation {
    fn default() -> Self {
        Self::new()
    }
}

// ─── ProviderType ─────────────────────────────────────────────────────────────

/// The type of LLM provider.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    /// Cloud API provider (OpenAI-compatible, Anthropic, etc.).
    Cloud,
    /// Local deployment API (Ollama, vLLM, etc.).
    Local,
    /// Direct model invocation via transformers/candle.
    Transformers,
}

// ─── CloudProviderKind ────────────────────────────────────────────────────────

/// Specific cloud provider protocol variant.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CloudProviderKind {
    /// OpenAI-compatible API protocol (covers OpenAI, DeepSeek, 智谱, etc.).
    OpenAI,
    /// Anthropic Claude native API protocol.
    Anthropic,
    /// Google Gemini native API protocol.
    Gemini,
}

// ─── ModelConfig ──────────────────────────────────────────────────────────────

/// Configuration for connecting to an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModelConfig {
    /// The type of provider.
    pub provider_type: ProviderType,

    /// Specific cloud provider kind (only relevant when provider_type == Cloud).
    #[serde(default)]
    pub cloud_kind: Option<CloudProviderKind>,

    /// API key for cloud providers.
    #[serde(default)]
    pub api_key: Option<String>,

    /// Base URL for the API endpoint.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Port number (for local deployments).
    #[serde(default)]
    pub port: Option<u16>,

    /// Model name / identifier.
    #[serde(default)]
    pub model_name: Option<String>,

    /// Local model path (for Transformers / local deployment).
    #[serde(default)]
    pub model_path: Option<String>,

    /// Additional provider-specific configuration.
    #[serde(default)]
    pub extra: HashMap<String, String>,
}

impl ModelConfig {
    /// Create an OpenAI-compatible cloud provider config.
    pub fn openai(api_key: String, model_name: String) -> Self {
        Self {
            provider_type: ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::OpenAI),
            api_key: Some(api_key),
            base_url: Some("https://api.openai.com/v1".to_string()),
            port: None,
            model_name: Some(model_name),
            model_path: None,
            extra: HashMap::new(),
        }
    }

    /// Create an OpenAI-compatible config with a custom base URL.
    pub fn openai_compatible(api_key: String, base_url: String, model_name: String) -> Self {
        Self {
            provider_type: ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::OpenAI),
            api_key: Some(api_key),
            base_url: Some(base_url),
            port: None,
            model_name: Some(model_name),
            model_path: None,
            extra: HashMap::new(),
        }
    }

    /// Create an Anthropic Claude cloud provider config.
    pub fn anthropic(api_key: String, model_name: String) -> Self {
        Self {
            provider_type: ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::Anthropic),
            api_key: Some(api_key),
            base_url: Some("https://api.anthropic.com/v1".to_string()),
            port: None,
            model_name: Some(model_name),
            model_path: None,
            extra: HashMap::new(),
        }
    }

    /// Create a Google Gemini cloud provider config.
    pub fn gemini(api_key: String, model_name: String) -> Self {
        Self {
            provider_type: ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::Gemini),
            api_key: Some(api_key),
            base_url: Some("https://generativelanguage.googleapis.com/v1beta".to_string()),
            port: None,
            model_name: Some(model_name),
            model_path: None,
            extra: HashMap::new(),
        }
    }

    /// Create an Ollama local provider config.
    pub fn ollama(model_name: String) -> Self {
        Self {
            provider_type: ProviderType::Local,
            cloud_kind: None,
            api_key: None,
            base_url: Some("http://localhost".to_string()),
            port: Some(11434),
            model_name: Some(model_name),
            model_path: None,
            extra: HashMap::new(),
        }
    }

    /// Create an Ollama config with a custom host/port.
    pub fn ollama_custom(host: String, port: u16, model_name: String) -> Self {
        Self {
            provider_type: ProviderType::Local,
            cloud_kind: None,
            api_key: None,
            base_url: Some(host),
            port: Some(port),
            model_name: Some(model_name),
            model_path: None,
            extra: HashMap::new(),
        }
    }

    /// Create a Transformers (local model) config.
    pub fn transformers(model_path: String) -> Self {
        Self {
            provider_type: ProviderType::Transformers,
            cloud_kind: None,
            api_key: None,
            base_url: None,
            port: None,
            model_name: None,
            model_path: Some(model_path),
            extra: HashMap::new(),
        }
    }

    /// Get the resolved API URL (base_url + port).
    pub fn resolved_url(&self) -> String {
        match (&self.base_url, &self.port) {
            (Some(url), Some(port)) => {
                // Avoid double-port if base_url already includes a port number
                let already_has_port = url.rsplit_once(':')
                    .map(|(_, rest)| rest.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false))
                    .unwrap_or(false);
                if already_has_port {
                    url.clone()
                } else {
                    format!("{url}:{port}")
                }
            }
            (Some(url), None) => url.clone(),
            (None, Some(port)) => format!("http://localhost:{port}"),
            (None, None) => String::new(),
        }
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider_type: ProviderType::Cloud,
            cloud_kind: None,
            api_key: None,
            base_url: None,
            port: None,
            model_name: None,
            model_path: None,
            extra: HashMap::new(),
        }
    }
}

// ─── ModelCapability ──────────────────────────────────────────────────────────

/// Describes the capabilities of a connected LLM model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCapability {
    /// Whether the model supports multimodal input (images, files).
    pub supports_multimodal: bool,

    /// Whether the model supports streaming responses.
    pub supports_streaming: bool,

    /// Whether the model supports tool/function calling.
    pub supports_tools: bool,

    /// The maximum context window size in tokens.
    pub context_window_size: u32,

    /// The maximum output tokens per response.
    pub max_output_tokens: u32,
}

impl ModelCapability {
    /// GPT-4 class capabilities.
    pub fn gpt4_class() -> Self {
        Self {
            supports_multimodal: true,
            supports_streaming: true,
            supports_tools: true,
            context_window_size: 128000,
            max_output_tokens: 4096,
        }
    }

    /// Claude class capabilities.
    pub fn claude_class() -> Self {
        Self {
            supports_multimodal: true,
            supports_streaming: true,
            supports_tools: true,
            context_window_size: 200000,
            max_output_tokens: 8192,
        }
    }

    /// Basic text-only model capabilities.
    pub fn basic_text() -> Self {
        Self {
            supports_multimodal: false,
            supports_streaming: true,
            supports_tools: false,
            context_window_size: 4096,
            max_output_tokens: 2048,
        }
    }
}

// ─── InferenceRequest ─────────────────────────────────────────────────────────

/// A request for LLM inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    /// The conversation to send to the model.
    pub conversation: Conversation,

    /// Tool definitions available for this request (JSON Schema format).
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,

    /// Maximum tokens to generate.
    #[serde(default)]
    pub max_tokens: Option<u32>,

    /// Temperature for sampling (0.0 = deterministic, 1.0 = creative).
    #[serde(default)]
    pub temperature: Option<f32>,

    /// Top-p for nucleus sampling.
    #[serde(default)]
    pub top_p: Option<f32>,

    /// Stop sequences.
    #[serde(default)]
    pub stop_sequences: Vec<String>,

    /// Whether to request constrained/structured output.
    #[serde(default)]
    pub constrained_output: Option<ConstrainedOutputConfig>,

    /// Token budget for extended thinking/reasoning.
    /// Anthropic uses this as `thinking.budget_tokens`; other providers may ignore it.
    /// When `None`, thinking is disabled. When `Some(N)`, providers that support thinking
    /// will allocate up to N tokens for the model's internal reasoning.
    #[serde(default)]
    pub thinking_budget: Option<u32>,

    /// Request metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

// ─── GenerationConfig ─────────────────────────────────────────────────────────

/// Sampling / generation parameters for LLM inference.
///
/// Holds the per-run knobs that control the model's output: temperature,
/// top-p, max_tokens, thinking budget, and stop sequences. All fields are
/// `Option` (stop_sequences defaults to empty) — when left `None`, the agent
/// loop applies a **scenario-appropriate built-in default** at the call site
/// (e.g. deterministic temperature for planning, moderate temperature for the
/// agentic tool-use loop) rather than letting the provider API's own default
/// leak through. Provider API defaults are frequently inappropriate for agents:
/// OpenAI and Anthropic both default `temperature` to 1.0, which is too random
/// for reliable tool-use / coding, and `thinking_budget` is Anthropic-specific
/// (it silently inflates `max_tokens` and is ignored by other providers).
///
/// Set a field to `Some(value)` to override the scenario default explicitly;
/// leave it `None` to inherit the scenario default.
///
/// Exposed via `AppBuilder::generation_config` / `temperature` / `top_p` /
/// `max_tokens` / `thinking_budget` / `stop_sequences`, then propagated into
/// the `AgentLoopConfig` that drives every inference call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GenerationConfig {
    /// Sampling temperature (0.0 = deterministic, 1.0 = creative).
    #[serde(default)]
    pub temperature: Option<f32>,

    /// Top-p (nucleus) sampling mass.
    #[serde(default)]
    pub top_p: Option<f32>,

    /// Maximum tokens to generate. `None` defers to the provider's model-aware
    /// default (providers know their own model ceilings; safer than a fixed
    /// agent-side cap that may exceed a model's max and error).
    #[serde(default)]
    pub max_tokens: Option<u32>,

    /// Token budget for extended thinking / reasoning.
    /// Anthropic maps this to `thinking.budget_tokens` (and bumps `max_tokens`
    /// to `budget + floor`); other providers ignore it. `None` = thinking off.
    #[serde(default)]
    pub thinking_budget: Option<u32>,

    /// Stop sequences — generation halts when any is emitted.
    #[serde(default)]
    pub stop_sequences: Vec<String>,
}

impl GenerationConfig {
    /// Create an empty config (all fields inherit scenario defaults).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the sampling temperature.
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Set the top-p (nucleus) mass.
    pub fn top_p(mut self, top_p: f32) -> Self {
        self.top_p = Some(top_p);
        self
    }

    /// Set the maximum output tokens.
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set the extended-thinking token budget (`None` disables thinking).
    pub fn thinking_budget(mut self, budget: Option<u32>) -> Self {
        self.thinking_budget = budget;
        self
    }

    /// Set the stop sequences.
    pub fn stop_sequences(mut self, stop_sequences: Vec<String>) -> Self {
        self.stop_sequences = stop_sequences;
        self
    }
}

// ─── InferenceResponse ────────────────────────────────────────────────────────

/// A complete (non-streaming) inference response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponse {
    /// The assistant's response message.
    pub message: Message,

    /// Token usage statistics.
    pub usage: TokenUsage,

    /// The model that produced this response.
    pub model: String,

    /// Response metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

// ─── TokenUsage ───────────────────────────────────────────────────────────────

/// Token usage statistics for an inference request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

// ─── InferenceStream ──────────────────────────────────────────────────────────

/// A streaming chunk from an SSE inference response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceStreamChunk {
    /// Content blocks received in this chunk.
    pub content: Vec<ContentBlock>,

    /// Whether this is the final chunk.
    pub is_final: bool,

    /// Token usage (only present in the final chunk).
    #[serde(default)]
    pub usage: Option<TokenUsage>,

    /// The model producing this chunk.
    #[serde(default)]
    pub model: Option<String>,
}

// ─── ToolDefinition ───────────────────────────────────────────────────────────

/// Definition of a tool available to the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    /// The tool name.
    pub name: String,

    /// Human-readable description of what the tool does.
    pub description: String,

    /// JSON Schema describing the tool's parameters.
    pub parameters_schema: serde_json::Value,
}

// ─── ToolOutput ───────────────────────────────────────────────────────────────

/// The output from a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Whether the tool execution succeeded.
    pub success: bool,

    /// The result content (text or JSON).
    pub content: String,

    /// Optional error message if execution failed.
    #[serde(default)]
    pub error: Option<String>,
}

// ─── RiskLevel (legacy) ────────────────────────────────────────────────────────

/// Risk level classification for tool execution approval (legacy).
///
/// **Deprecated**: Use `PermissionLevel` instead. This enum is retained
/// for backward compatibility with existing code and will be removed
/// in a future version.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    /// Low risk — safe to execute automatically.
    Low,
    /// Medium risk — may require human review.
    Medium,
    /// High risk — must be approved by human before execution.
    High,
}

// ─── PermissionLevel ────────────────────────────────────────────────────────

/// Permission level classification for tool execution (replaces RiskLevel).
///
/// Three-tier system inspired by Claude Code's Read/Standard/Full:
/// - **Read**: Only-observe operations (file reading, search, environment sensing).
///   These tools never modify state and are always auto-approved.
/// - **Standard**: Common operations (file editing, tool calling, MCP interaction).
///   These tools modify state but are generally safe with reasonable constraints.
/// - **Full**: Powerful operations (shell execution, file deletion, system commands).
///   These tools can cause significant changes and require explicit approval.
///
/// Tools are automatically categorized by their operation type:
/// - FileReadTool, GrepTool, GlobTool, EnvironmentTool → Read
/// - FileEditTool, FileWriteTool, NotebookEditTool → Standard
/// - ShellTool, FileDeleteTool → Full
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PermissionLevel {
    /// Read — only-observe operations. Auto-approved.
    Read,
    /// Standard — common operations. May require approval depending on policy.
    Standard,
    /// Full — powerful operations. Always requires approval.
    Full,
}

impl PermissionLevel {
    /// Convert from legacy RiskLevel.
    pub fn from_risk_level(risk: RiskLevel) -> Self {
        match risk {
            RiskLevel::Low => Self::Read,
            RiskLevel::Medium => Self::Standard,
            RiskLevel::High => Self::Full,
        }
    }

    /// Convert to legacy RiskLevel.
    pub fn to_risk_level(self) -> RiskLevel {
        match self {
            Self::Read => RiskLevel::Low,
            Self::Standard => RiskLevel::Medium,
            Self::Full => RiskLevel::High,
        }
    }

    /// Whether this permission level should be auto-approved
    /// given the configured approval threshold.
    pub fn should_auto_approve(&self, threshold: &PermissionLevel) -> bool {
        match (self, threshold) {
            (Self::Read, Self::Read) => true,
            (Self::Read, Self::Standard) => true,
            (Self::Read, Self::Full) => true,
            (Self::Standard, Self::Read) => false,
            (Self::Standard, Self::Standard) => true,
            (Self::Standard, Self::Full) => true,
            (Self::Full, Self::Read) => false,
            (Self::Full, Self::Standard) => false,
            (Self::Full, Self::Full) => true,
        }
    }
}

// ─── ApprovalRequest / ApprovalResponse ───────────────────────────────────────

/// Request for human approval of a high-risk tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The name of the tool requesting approval.
    pub tool_name: String,

    /// The arguments the tool wants to execute with.
    pub args: serde_json::Value,

    /// The risk level classification (legacy — use permission_level).
    pub risk_level: RiskLevel,

    /// The permission level classification (new — replaces risk_level).
    #[serde(default)]
    pub permission_level: Option<PermissionLevel>,

    /// Justification for why the tool should be allowed to execute.
    pub justification: String,
}

/// User response to an approval request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApprovalResponse {
    /// Approved — allow execution (possibly with modified args).
    Approved {
        modified_args: Option<serde_json::Value>,
    },
    /// Denied — block execution.
    Denied {
        reason: String,
    },
    /// Modified — allow execution with different arguments.
    Modified {
        args: serde_json::Value,
    },
    /// Observe — pause execution and observe the agent's current state.
    ///
    /// This enables the "observation mode" (Issue #17):
    /// humans can view the agent's state flow in real-time
    /// and decide to continue, terminate, or modify at any point.
    ///
    /// When the AgentLoop receives an Observe response:
    /// 1. Execution pauses
    /// 2. The current LoopState snapshot is emitted to the UI
    /// 3. The user decides: Continue / Terminate / Modify
    Observe {
        /// The user's observation comment (optional).
        observation: String,
    },
}

// ─── PlanStep / PlanStepStatus ───────────────────────────────────────────────
//
// `PlanStep` lives in `oneai-core` (not `oneai-agent`) so that
// `InteractionRequest::PlanReview` — whose signature is part of the
// `InteractionGate` trait defined in core — can reference it without a
// layering violation (core must not depend on agent). The `plan_agent`
// crate re-exports these for backward-compatible `oneai_agent::PlanStep`
// access.

/// A single step in a task plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    /// Step identifier (e.g., "step_1").
    pub id: String,

    /// Brief description of what this step should accomplish.
    pub description: String,

    /// Whether this step is coupled to previous steps.
    /// - Coupled: must wait for previous step's result → ReAct pipeline
    /// - Non-coupled: can run independently → parallel sub-agent
    pub coupled: bool,

    /// Which previous step IDs this step depends on (if coupled).
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// Execution status of this step. Defaults to Pending when created by the
    /// planner; mutated by the `task_update` tool during execution so the TUI
    /// can render live progress.
    #[serde(default)]
    pub status: PlanStepStatus,

    /// Present-continuous label shown in the spinner while the step is in
    /// progress (e.g. "Running tests"). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

/// Execution status of a plan step.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    /// Not started yet.
    #[default]
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Failed or aborted.
    Failed,
}

impl PlanStepStatus {
    /// Icon for TUI display.
    pub fn icon(&self) -> &'static str {
        match self {
            PlanStepStatus::Pending => "○",
            PlanStepStatus::InProgress => "◐",
            PlanStepStatus::Completed => "●",
            PlanStepStatus::Failed => "✗",
        }
    }
}

// ─── InteractionGate types ───────────────────────────────────────────────────
//
// Unified "agent loop suspends → asks the application layer → resumes with a
// reply" surface. Replaces the split between `ApprovalGate` (tool approve/deny),
// `on_plan_submitted` (bool plan accept), and the dead PreInfer/PostInfer
// `LifecycleHook` interactive path. The application layer decides per-point
// whether to actually call back to the UI (`enabled(point)`); all other points
// short-circuit to `Proceed` with zero latency.

/// Which decision point in the agent loop an interaction request originates from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InteractionPoint {
    /// Before an LLM inference call.
    PreInfer,
    /// After an LLM inference call.
    PostInfer,
    /// High-risk tool approval.
    ToolApproval,
    /// A tradeoff decision surfaced during planning (before the final plan).
    PlanDecision,
    /// Final confirmation of a single produced plan.
    PlanReview,
}

/// A selectable option for a [`InteractionRequest::PlanDecision`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionOption {
    /// Stable id, e.g. `"opt_a"`.
    pub id: String,
    /// Short human label, e.g. `"优先速度"`.
    pub label: String,
    /// What this option means.
    pub description: String,
    /// The cost / tradeoff this option incurs, stated honestly.
    pub tradeoffs: String,
}

/// A request from the agent loop to the application layer at a decision point.
///
/// The loop blocks on the corresponding [`InteractionResponse`] before resuming.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum InteractionRequest {
    /// LLM inference is about to run. The layer may rewrite the request
    /// (inject context / filter tools), ask for a feedback-grounded retry,
    /// or skip this iteration.
    PreInfer {
        request: InferenceRequest,
        iteration: usize,
        paradigm: String,
    },
    /// LLM inference just returned. The layer may validate / filter / replace
    /// the response, or ask for a feedback-grounded retry.
    PostInfer {
        response: InferenceResponse,
        request: InferenceRequest,
        iteration: usize,
        paradigm: String,
    },
    /// A high-risk tool is about to execute — replaces `ApprovalRequest`'s
    /// interactive role.
    ToolApproval {
        approval: ApprovalRequest,
    },
    /// The planner hit a tradeoff with no clearly superior option and asks the
    /// user to choose (or supply a custom decision) before producing the final
    /// plan. Surfaces before [`InteractionRequest::PlanReview`].
    PlanDecision {
        decision_id: String,
        /// The question requiring a decision (e.g. "优先速度还是正确性？").
        question: String,
        /// Why the user's input is needed at this point.
        context: String,
        options: Vec<DecisionOption>,
    },
    /// Final confirmation of a single produced plan.
    PlanReview {
        plan: String,
        steps: Vec<PlanStep>,
    },
}

/// The application layer's reply to an [`InteractionRequest`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum InteractionResponse {
    /// Proceed without changes.
    Proceed,
    /// Proceed but apply a modification (PreInfer rewrites request, PostInfer
    /// replaces response, ToolApproval rewrites args, PlanReview uses a
    /// user-edited plan).
    ProceedWith {
        modification: InteractionModification,
    },
    /// Reject and feed back free-text corrective guidance. The loop injects the
    /// feedback into the conversation and retries / re-plans.
    Revise {
        feedback: String,
    },
    /// `PlanDecision`-only: select an option by its `id`.
    Choose {
        option_id: String,
    },
    /// Abort this iteration or the whole loop.
    Abort {
        reason: String,
    },
}

/// A modification attached to [`InteractionResponse::ProceedWith`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum InteractionModification {
    /// PreInfer: inject a system message into the conversation.
    InjectSystemMessage(String),
    /// PreInfer: replace the inference request entirely.
    ReplaceRequest(InferenceRequest),
    /// PostInfer: replace the inference response entirely.
    ReplaceResponse(InferenceResponse),
    /// ToolApproval: replace the tool arguments.
    ReplaceToolArgs(serde_json::Value),
    /// PlanReview: the user edited the plan directly in the UI.
    ReplacePlan {
        plan: String,
        steps: Vec<PlanStep>,
    },
}

// ─── ConstrainedOutputConfig ──────────────────────────────────────────────────

/// Configuration for constrained/structured output (Layer 1 of the parser).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstrainedOutputConfig {
    /// JSON Schema that the output must conform to.
    pub schema: serde_json::Value,

    /// The constrained decoding mode to use.
    pub mode: ConstrainedMode,
}

/// Mode for constrained decoding.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConstrainedMode {
    /// BNF grammar-constrained decoding (LiteRT-LM, Ollama).
    BnfGrammar,
    /// JSON Schema-constrained decoding.
    JsonSchema,
    /// Regex-constrained decoding.
    Regex,
}

// ─── ParsedOutput ─────────────────────────────────────────────────────────────

/// Output from the parser after applying the 3-layer defense.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedOutput {
    /// The successfully parsed content blocks.
    pub content: Vec<ContentBlock>,

    /// Which parsing layer succeeded.
    pub parsing_layer: ParsingLayer,

    /// Number of fallback retries if Layer 3 was used.
    #[serde(default)]
    pub fallback_retries: usize,
}

/// Which layer of the 3-layer parser defense succeeded.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParsingLayer {
    /// Layer 1: Constrained decoding (BNF grammar) — output was guaranteed correct at generation.
    ConstrainedDecoding,
    /// Layer 2: Fuzzy JSON repair — the raw output was malformed but repairable.
    FuzzyRepair,
    /// Layer 3: Fallback self-correction — model re-generated correct output after error feedback.
    FallbackSelfCorrection,
}

// ─── MemoryEntry / MemoryQuery ────────────────────────────────────────────────

/// An entry in the memory system.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryEntry {
    /// Unique identifier.
    pub id: String,

    /// The content of this memory entry.
    pub content: String,

    /// Timestamp when this entry was created.
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Optional vector embedding for semantic search.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,

    /// Source metadata (which conversation, which agent, etc.).
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// A query to the memory system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryQuery {
    /// Text query for semantic search.
    pub text: String,

    /// Optional vector embedding (if pre-computed).
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,

    /// Time range filter.
    #[serde(default)]
    pub time_range: Option<TimeRange>,

    /// Metadata filters.
    #[serde(default)]
    pub metadata_filters: HashMap<String, String>,
}

/// Time range for memory queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
}

// ─── Memory recall strategy (canonical home) ─────────────────────────────────

/// Strategy for recalling memories from LTM/archival into context.
///
/// Different strategies are suited for different scenarios:
/// - KeywordFirst: works without embeddings (faster, simpler)
/// - SemanticFirst: requires embeddings (more relevant, deeper)
/// - Hybrid: combines both (best coverage, aligned with HybridScorer)
///
/// This is the canonical definition; `oneai-memory` re-exports it so the
/// domain-level `MemoryProfile` and the runtime `MemoryManager` share one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallStrategy {
    /// Keyword search first, then semantic if available.
    KeywordFirst,
    /// Semantic (embedding) search first, then keyword as fallback.
    SemanticFirst,
    /// Both keyword and semantic search, merge and deduplicate.
    Hybrid,
}

impl Default for RecallStrategy {
    fn default() -> Self {
        Self::Hybrid
    }
}

// ─── Memory facts & domain memory profile types ──────────────────────────────
//
// These types back the DomainPack "MemoryProfile" layer — a declarative,
// domain-scoped memory policy (what to extract as durable facts, how to
// recall them, what counts as a cross-session user habit). They live in
// `oneai-core` so both `oneai-domain` (declarative profile) and
// `oneai-memory` (runtime tiers) depend on the same definitions.

/// A category label for an atomic memory fact, scoped by a DomainPack's
/// extraction schema.
///
/// Examples:
/// - Coding: `user_tooling_pref`, `decision`, `open_task`, `critical_file`
/// - Research: `source`, `claim`, `open_question`, `user_interest`
///
/// Fact types that also appear in a profile's `habit_fact_types` are persisted
/// under the **user** namespace and recalled across sessions ("越用越好用").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FactType(pub String);

impl FactType {
    /// Create a fact type from any string-like input.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The category label as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for FactType {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for FactType {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for FactType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Scope at which a memory fact is persisted and recalled.
///
/// `User` facts (e.g. preferences, habits) survive across sessions for the
/// same user; `Session` facts (e.g. episodic context) are scoped to one
/// conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MemoryScope {
    /// Cross-session, per-user (habits/preferences).
    User,
    /// Per-session (episodic / working context).
    Session,
}

/// Domain-level recall configuration, carried by `MemoryProfile`.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallConfig {
    /// Which recall strategy to use (keyword / semantic / hybrid).
    pub strategy: RecallStrategy,
    /// Maximum number of facts to recall per turn.
    pub top_k: usize,
    /// Whether to apply time-decay weighting during recall.
    pub time_decay: bool,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            strategy: RecallStrategy::Hybrid,
            top_k: 5,
            time_decay: true,
        }
    }
}

/// An atomic memory fact — the unit of long-term / archival memory.
///
/// Extracted from conversation by the (compression-coupled) `FactExtractor`,
/// or explicitly stored by the agent via self-managed memory tools. Facts are
/// conflict-resolved by `(user_id, subject, predicate)`: a contradicting new
/// fact **updates** the existing one (bumping `version` and `updated_at`)
/// rather than appending a duplicate — the Mem0-style invariant that keeps
/// archival memory from drifting into contradiction over long use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryFact {
    /// Unique identifier.
    pub id: String,

    /// Owning user (cross-session habit namespace). Empty string when unset.
    #[serde(default)]
    pub user_id: String,

    /// Owning session (episodic namespace). Empty string when unset.
    #[serde(default)]
    pub session_id: String,

    /// Category from the DomainPack's extraction schema.
    pub fact_type: FactType,

    /// What the fact is about (e.g. "user.package_manager", "auth.module").
    pub subject: String,

    /// The relationship/assertion (e.g. "prefers", "decided_to", "status_is").
    pub predicate: String,

    /// The fact's value/content (e.g. "pnpm", "use JWT", "in_progress").
    pub content: String,

    /// Optional embedding for semantic recall.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,

    /// Arbitrary metadata (source turn, confidence, etc.).
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    /// Salience/importance score in `[0.0, 1.0]` used by the three-factor
    /// recall scorer (recency + importance + relevance, à la Generative
    /// Agents). Higher = more likely to be recalled. Defaults to `0.5`;
    /// `FactExtractor` assigns per-category defaults (decisions/episodics
    /// higher, open tasks medium) and the agent can revise it via the
    /// self-managed memory tools.
    #[serde(default = "default_importance")]
    pub importance: f32,

    /// When the fact was first recorded.
    pub created_at: chrono::DateTime<chrono::Utc>,

    /// When the fact was last updated (== created_at on first write).
    pub updated_at: chrono::DateTime<chrono::Utc>,

    /// Monotonic version counter, incremented on each conflict-resolved update.
    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    1
}

/// Default importance for a fact lacking an explicit salience (see `MemoryFact`).
fn default_importance() -> f32 {
    0.5
}

impl MemoryFact {
    /// The (user_id, subject, predicate) key used for conflict resolution.
    ///
    /// Two facts with the same key are the "same fact"; a new value updates
    /// rather than duplicates.
    pub fn conflict_key(&self) -> (&str, &str, &str) {
        (&self.user_id, &self.subject, &self.predicate)
    }
}

// ─── SkillDescriptor ──────────────────────────────────────────────────────────

/// Description of a SKILL that can be dynamically injected into agent context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDescriptor {
    /// Unique skill name.
    pub name: String,

    /// Human-readable description.
    pub description: String,

    /// Prompt template for progressive disclosure.
    pub prompt_template: String,

    /// Keywords for fast matching.
    #[serde(default)]
    pub trigger_keywords: Vec<String>,

    /// Pre-computed embedding for vector matching.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
}

// ─── SelectionMode ────────────────────────────────────────────────────────────

/// Mode for skill/memory selection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMode {
    /// Pure keyword matching (fastest, lowest quality).
    KeywordMatch,
    /// Vector similarity matching (slower, higher quality).
    VectorSimilarity,
    /// Hybrid: keyword pre-filter + vector ranking.
    Hybrid,
}

// ─── Utility functions ─────────────────────────────────────────────────────

/// Check if a text contains a keyword (case-insensitive substring match).
///
/// This is the standard keyword matching algorithm used across all OneAI
/// search/retrieval subsystems. Previously implemented independently in
/// short-term memory, long-term memory, content store, RAG index, and skill selector.
pub fn keyword_matches(text: &str, keyword: &str) -> bool {
    text.to_lowercase().contains(&keyword.to_lowercase())
}

// ─── Lifecycle Hooks ──────────────────────────────────────────────────────────

/// Hook point — when in the agent lifecycle a hook is triggered.
///
/// Inspired by Claude Code's hooks system (PreToolUse/PostToolUse/Notification/Stop),
/// this extends the model to include inference lifecycle hooks as well.
/// This represents the evolution from "围栏式安全" (ApprovalGate — execution gate)
/// to "生命周期安全" (LifecycleHook — event-driven policy at every stage).
///
/// Hook points and their purposes:
/// - **PreToolUse**: Inspect/modify/deny tool calls before execution. This replaces
///   some ApprovalGate use cases with programmatic hooks (e.g., CI/CD auto-approve
///   read tools, deny dangerous commands).
/// - **PostToolUse**: Audit/log/transform tool outputs after execution. Used for
///   compliance logging, output sanitization, or result enrichment.
/// - **PreInfer**: Modify the inference request before sending to the LLM. Used for
///   context injection (add safety reminders, domain constraints) or request filtering.
/// - **PostInfer**: Inspect/modify the inference response after receiving it. Used for
///   content filtering, response validation, or logging.
/// - **PreCheckpoint**: Inspect/modify state before checkpointing. Used for state
///   sanitization or selective checkpoint policies.
/// - **Notification**: General notification event (not a decision point). Used for
///   progress tracking, metrics collection, or external system alerts.
/// - **Stop**: Final hook before the loop terminates. Used for cleanup, final logging,
///   or state persistence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    /// Before a tool is executed — can allow/deny/modify the tool call.
    PreToolUse,
    /// After a tool has been executed — can audit/log/transform the output.
    PostToolUse,
    /// Before LLM inference — can modify the request (inject context, filter).
    PreInfer,
    /// After LLM inference — can modify the response (filter content, validate).
    PostInfer,
    /// Before a checkpoint is saved — can modify/skip the checkpoint.
    PreCheckpoint,
    /// General notification event — informational, not a decision point.
    Notification,
    /// Before the loop terminates — cleanup/final logging.
    Stop,
}

/// Hook result — the outcome of running a lifecycle hook.
///
/// This mirrors Claude Code's allow/deny/modify tri-state:
/// - **Allow**: Proceed without changes (the default for audit/logging hooks).
/// - **Deny**: Block the action (the hook vetoes the operation).
/// - **Modify**: Proceed but with changed parameters (the hook transforms the input).
///
/// For PreToolUse hooks:
/// - Allow → tool executes with original args
/// - Deny → tool is not executed, error message injected
/// - Modify → tool executes with modified_args
///
/// For PreInfer hooks:
/// - Allow → request sent unchanged
/// - Deny → inference skipped (rare, mainly for safety constraints)
/// - Modify → request sent with modifications (extra context, filtered tools)
///
/// For PostToolUse/PostInfer hooks:
/// - Allow → output/response passed through unchanged
/// - Deny → output/response replaced with error message
/// - Modify → output/response replaced with modified_args content
#[derive(Debug, Clone)]
pub enum HookResult {
    /// Allow the action to proceed without modification.
    Allow,

    /// Deny (block) the action with a reason.
    /// The reason is injected into the conversation as an error message.
    Deny { reason: String },

    /// Allow the action but with modified parameters.
    /// The modified_args replace the original parameters.
    Modify { modified_args: serde_json::Value },
}

// ─── HookContext ───────────────────────────────────────────────────────────────

/// Context provided to a lifecycle hook when it runs.
///
/// Contains the relevant data for the hook point — not all fields
/// are populated for every point. The hook should check which fields
/// are relevant for its registered point(s).
///
/// Example: a PreToolUse hook receives `tool_name` and `tool_args`,
/// but not `tool_output` (the tool hasn't executed yet).
#[derive(Debug, Clone)]
pub struct HookContext {
    /// Which hook point triggered this call.
    pub point: HookPoint,

    /// The tool name (populated for PreToolUse/PostToolUse).
    pub tool_name: Option<String>,

    /// The tool arguments (populated for PreToolUse — may be modified by hook).
    pub tool_args: Option<serde_json::Value>,

    /// The tool output (populated for PostToolUse — may be transformed by hook).
    pub tool_output: Option<ToolOutput>,

    /// The inference request (populated for PreInfer — may be modified by hook).
    pub inference_request: Option<InferenceRequest>,

    /// The inference response (populated for PostInfer — may be modified by hook).
    pub inference_response: Option<InferenceResponse>,

    /// The current loop iteration number.
    pub iteration: usize,

    /// The active paradigm name.
    pub paradigm: String,
}

// ─── Interrupt/Resume ──────────────────────────────────────────────────────────

/// An interrupt point in the agent loop.
///
/// When the loop is interrupted, it pauses at an iteration boundary,
/// saves the LoopState, and returns a partial result. The interrupt
/// can later be resumed by injecting human feedback and continuing execution.
///
/// This is the HITL evolution from "审批门" (ApprovalGate — gate-based pause)
/// to "暂停恢复" (Interrupt — arbitrary-point pause with feedback injection).
/// Inspired by LangGraph's interrupt() + Command(resume) pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptPoint {
    /// Unique interrupt ID (for resuming).
    pub id: String,

    /// The iteration at which the interrupt occurred.
    pub iteration: usize,

    /// The reason for the interrupt.
    pub reason: InterruptReason,

    /// The checkpoint ID for resuming from this interrupt (if checkpointing is enabled).
    pub checkpoint_id: Option<String>,
}

/// Why the loop was interrupted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum InterruptReason {
    /// Human approval needed for a tool call (from ApprovalGate or LifecycleHook).
    HumanApprovalNeeded {
        tool_name: String,
        args: serde_json::Value,
    },

    /// Human feedback requested — the agent wants guidance before proceeding.
    HumanFeedbackRequested {
        question: String,
    },

    /// Paradigm boundary — pause at paradigm switch for human review.
    ParadigmBoundary {
        from: String,
        to: String,
    },

    /// Custom interrupt reason (user-defined).
    Custom {
        reason: String,
    },
}

/// Resume signal — injected when the loop resumes from an interrupt.
///
/// Contains the human's feedback and the action to take:
/// continue as-is, modify the approach, or stop entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeSignal {
    /// The interrupt ID being resumed from.
    pub interrupt_id: String,

    /// Human feedback text to inject into the conversation.
    pub feedback: String,

    /// What to do when resuming.
    pub action: ResumeAction,
}

/// What to do when resuming from an interrupt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ResumeAction {
    /// Continue execution as planned (just inject the feedback).
    Continue,

    /// Modify the current approach based on feedback.
    Modify {
        modified_args: Option<serde_json::Value>,
    },

    /// Stop the loop entirely (human decided to abort).
    Stop,
}

// ─── StructuredOutput + ModelRetry ─────────────────────────────────────────────

/// Configuration for structured output validation with automatic retry.
///
/// When the model's final output doesn't conform to the specified JSON Schema,
/// the AgentLoop can automatically re-prompt the model with the validation error
/// for self-correction. This is the "Rust 版 PydanticAI" pattern — leveraging
/// Rust's type safety for output quality assurance.
///
/// The validation happens at the DirectAnswer stage (after the loop decides
/// the model has produced a final answer). If validation fails:
/// 1. The error details are injected as a system message
/// 2. The loop continues (without incrementing the iteration counter)
/// 3. The model re-generates its output with the error feedback
/// 4. Repeat until validation passes or max_retries is exhausted
///
/// Retry attempts don't count against the hard_max_iterations budget,
/// since they're self-correction attempts, not new task iterations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StructuredOutputConfig {
    /// JSON Schema that the model's final output must conform to.
    pub schema: serde_json::Value,

    /// Maximum retry attempts when validation fails.
    pub max_retries: usize,

    /// Whether to re-prompt with validation error (ModelRetry pattern).
    /// When true, validation failures trigger a re-prompt with the error.
    /// When false, validation failures are treated as final (loop ends with error).
    pub re_prompt_on_failure: bool,

    /// Custom validation error prompt template.
    /// If None, a default template is used:
    /// "Your previous output did not conform to the required schema.
    ///  Errors: {errors}. Please re-generate your output conforming to the schema."
    pub error_prompt_template: Option<String>,
}

/// Model retry information — injected into re-prompt context when
/// structured output validation fails.
///
/// This is inspired by PydanticAI's ModelRetry pattern:
/// when a model's output fails validation, the error context is
/// fed back to the model for self-correction. The model sees:
/// - What it produced (failed_output)
/// - What went wrong (error_message)
/// - What was expected (expected_schema)
/// - How many retries have happened (retry_count)
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRetry {
    /// The validation error message (what went wrong).
    pub error_message: String,

    /// How many retry attempts have been made so far.
    pub retry_count: usize,

    /// The JSON Schema that was expected.
    pub expected_schema: serde_json::Value,

    /// The actual output that failed validation.
    pub failed_output: String,
}

// ─── GraphDecision ──────────────────────────────────────────────────────────────

/// A structured decision parsed from LLM output in a StateGraph execution context.
///
/// This is the lightweight version of `AgentDecision` (from `oneai-agent`),
/// placed in `oneai-core` to avoid circular dependencies. `oneai-workflow` uses
/// this in `GraphState.parsed_decision` for edge condition routing, and
/// `oneai-agent`'s `AgentLoopGraphActionExecutor` maps `AgentDecision` →
/// `GraphDecision` when bridging between the two execution models.
///
/// The four variants correspond to the four outcomes an LLM can produce:
/// - **DirectAnswer**: The model produced a final text answer (no tool calls).
/// - **ToolCalls**: The model wants to invoke one or more tools.
/// - **Delegate**: The model wants to delegate a subtask to a specialized sub-agent.
/// - **SwitchParadigm**: The model wants to switch to a different paradigm.
///
/// This enables StateGraph edge conditions to route based on structured decisions
/// rather than unreliable string pattern matching (e.g., checking for "tool_use"
/// in raw text).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "decision_type")]
pub enum GraphDecision {
    /// The model produced a final answer — no tool calls, no delegation.
    #[serde(rename = "direct_answer")]
    DirectAnswer {
        /// The answer text.
        text: String,
    },

    /// The model wants to invoke one or more tools.
    #[serde(rename = "tool_calls")]
    ToolCalls {
        /// Number of tool calls requested.
        count: usize,
    },

    /// The model wants to delegate a subtask to a specialized sub-agent.
    #[serde(rename = "delegate")]
    Delegate {
        /// The sub-agent kind (e.g., "explore", "code", "review").
        agent_kind: String,
        /// The task description for the sub-agent.
        task: String,
    },

    /// The model wants to switch to a different paradigm.
    #[serde(rename = "switch_paradigm")]
    SwitchParadigm {
        /// The target paradigm (e.g., "plan", "react", "reflect", "explore").
        paradigm: String,
    },
}

impl GraphDecision {
    /// Check if this decision is a direct answer (loop should end).
    pub fn is_final(&self) -> bool {
        matches!(self, Self::DirectAnswer { .. })
    }

    /// Check if this decision involves tool calls.
    pub fn has_tool_calls(&self) -> bool {
        matches!(self, Self::ToolCalls { .. })
    }

    /// Check if this decision involves delegation.
    pub fn is_delegation(&self) -> bool {
        matches!(self, Self::Delegate { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_config_builder_and_default() {
        let empty = GenerationConfig::new();
        assert_eq!(empty.temperature, None);
        assert_eq!(empty.top_p, None);
        assert_eq!(empty.max_tokens, None);
        assert_eq!(empty.thinking_budget, None);
        assert!(empty.stop_sequences.is_empty());

        let cfg = GenerationConfig::new()
            .temperature(0.3)
            .top_p(0.95)
            .max_tokens(4096)
            .thinking_budget(Some(12000))
            .stop_sequences(vec!["<stop>".to_string()]);
        assert_eq!(cfg.temperature, Some(0.3));
        assert_eq!(cfg.top_p, Some(0.95));
        assert_eq!(cfg.max_tokens, Some(4096));
        assert_eq!(cfg.thinking_budget, Some(12000));
        assert_eq!(cfg.stop_sequences, vec!["<stop>".to_string()]);
    }

    #[test]
    fn generation_config_round_trips_through_serde() {
        let cfg = GenerationConfig::new().temperature(0.7).max_tokens(2048);
        let json = serde_json::to_string(&cfg).unwrap();
        let back: GenerationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.temperature, Some(0.7));
        assert_eq!(back.max_tokens, Some(2048));
        assert_eq!(back.top_p, None);
    }
}
