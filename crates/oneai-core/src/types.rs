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