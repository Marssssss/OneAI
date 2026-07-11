//! UniFFI-specific view types for cross-platform binding exports.
//!
//! These types mirror the core OneAI types but use UniFFI-compatible
//! representations. The key difference: `serde_json::Value` is replaced
//! with `String` (JSON-encoded), since UniFFI cannot export complex
//! recursive enums like serde_json::Value.
//!
//! All view types derive `uniffi::Record` or `uniffi::Enum` for
//! automatic foreign-language binding generation.

// ─── RiskLevelView ──────────────────────────────────────────────────

/// Risk level classification (UniFFI-compatible view).
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum RiskLevelView {
    /// Low risk — safe to execute automatically.
    Low,
    /// Medium risk — may require human review.
    Medium,
    /// High risk — must be approved by human before execution.
    High,
}

impl From<oneai_core::RiskLevel> for RiskLevelView {
    fn from(level: oneai_core::RiskLevel) -> Self {
        match level {
            oneai_core::RiskLevel::Low => RiskLevelView::Low,
            oneai_core::RiskLevel::Medium => RiskLevelView::Medium,
            oneai_core::RiskLevel::High => RiskLevelView::High,
        }
    }
}

impl From<RiskLevelView> for oneai_core::RiskLevel {
    fn from(view: RiskLevelView) -> Self {
        match view {
            RiskLevelView::Low => oneai_core::RiskLevel::Low,
            RiskLevelView::Medium => oneai_core::RiskLevel::Medium,
            RiskLevelView::High => oneai_core::RiskLevel::High,
        }
    }
}

// ─── PermissionLevelView ────────────────────────────────────────────

/// Permission level classification (UniFFI-compatible view).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum PermissionLevelView {
    /// Read — only-observe operations. Auto-approved.
    Read,
    /// Standard — common operations. May require approval.
    Standard,
    /// Full — powerful operations. Always requires approval.
    Full,
}

impl From<oneai_core::PermissionLevel> for PermissionLevelView {
    fn from(level: oneai_core::PermissionLevel) -> Self {
        match level {
            oneai_core::PermissionLevel::Read => PermissionLevelView::Read,
            oneai_core::PermissionLevel::Standard => PermissionLevelView::Standard,
            oneai_core::PermissionLevel::Full => PermissionLevelView::Full,
        }
    }
}

impl From<PermissionLevelView> for oneai_core::PermissionLevel {
    fn from(view: PermissionLevelView) -> Self {
        match view {
            PermissionLevelView::Read => oneai_core::PermissionLevel::Read,
            PermissionLevelView::Standard => oneai_core::PermissionLevel::Standard,
            PermissionLevelView::Full => oneai_core::PermissionLevel::Full,
        }
    }
}

// ─── ApprovalRequestView ────────────────────────────────────────────

/// Approval request (UniFFI-compatible view).
///
/// `args_json` is a JSON-encoded string of the tool arguments,
/// since `serde_json::Value` cannot be exported via UniFFI.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ApprovalRequestView {
    /// The name of the tool requesting approval.
    pub tool_name: String,
    /// JSON-encoded tool arguments.
    pub args_json: String,
    /// The risk level classification.
    pub risk_level: RiskLevelView,
    /// The permission level classification (optional — replaces risk_level).
    pub permission_level: Option<PermissionLevelView>,
    /// Justification for why the tool should be allowed.
    pub justification: String,
}

impl From<oneai_core::ApprovalRequest> for ApprovalRequestView {
    fn from(req: oneai_core::ApprovalRequest) -> Self {
        Self {
            tool_name: req.tool_name,
            args_json: req.args.to_string(),
            risk_level: RiskLevelView::from(req.risk_level),
            permission_level: req.permission_level.map(PermissionLevelView::from),
            justification: req.justification,
        }
    }
}

impl From<ApprovalRequestView> for oneai_core::ApprovalRequest {
    fn from(view: ApprovalRequestView) -> Self {
        Self {
            tool_name: view.tool_name,
            args: serde_json::from_str(&view.args_json)
                .unwrap_or(serde_json::json!({})),
            risk_level: oneai_core::RiskLevel::from(view.risk_level),
            permission_level: view.permission_level.map(oneai_core::PermissionLevel::from),
            justification: view.justification,
        }
    }
}

// ─── ToolOutputView ─────────────────────────────────────────────────

/// Tool execution output (UniFFI-compatible view).
#[derive(Debug, Clone, uniffi::Record)]
pub struct ToolOutputView {
    /// Whether the tool execution succeeded.
    pub success: bool,
    /// The result content (text or JSON).
    pub content: String,
    /// Optional error message if execution failed.
    pub error: Option<String>,
}

impl From<oneai_core::ToolOutput> for ToolOutputView {
    fn from(output: oneai_core::ToolOutput) -> Self {
        Self {
            success: output.success,
            content: output.content,
            error: output.error,
        }
    }
}

impl From<ToolOutputView> for oneai_core::ToolOutput {
    fn from(view: ToolOutputView) -> Self {
        Self {
            success: view.success,
            content: view.content,
            error: view.error,
        }
    }
}

// ─── ContentBlockView ───────────────────────────────────────────────

/// Content block (UniFFI-compatible view).
///
/// Represents multimodal content units in the OneAI framework.
/// `Image.data` uses `Vec<u8>` which UniFFI supports natively.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum ContentBlockView {
    /// Plain text content.
    Text {
        text: String,
    },
    /// Image content with raw bytes.
    Image {
        mime_type: String,
        data: Vec<u8>,
    },
    /// File reference by URI.
    File {
        mime_type: String,
        uri: String,
    },
    /// A tool call request from the model.
    ToolCall {
        id: String,
        name: String,
        /// JSON-encoded tool arguments.
        args_json: String,
    },
    /// The result of a tool call.
    ToolResult {
        call_id: String,
        content: String,
    },
    /// Thinking/reasoning content from extended thinking models.
    Thinking {
        text: String,
    },
}

impl From<oneai_core::ContentBlock> for ContentBlockView {
    fn from(block: oneai_core::ContentBlock) -> Self {
        match block {
            oneai_core::ContentBlock::Text { text } => ContentBlockView::Text { text },
            oneai_core::ContentBlock::Image { mime_type, data } => ContentBlockView::Image { mime_type, data },
            oneai_core::ContentBlock::File { mime_type, uri } => ContentBlockView::File { mime_type, uri },
            oneai_core::ContentBlock::ToolCall { id, name, args } => {
                ContentBlockView::ToolCall { id, name, args_json: args }
            },
            oneai_core::ContentBlock::ToolResult { call_id, content } => {
                ContentBlockView::ToolResult { call_id, content }
            },
            oneai_core::ContentBlock::Thinking { text } => {
                ContentBlockView::Thinking { text }
            },
            _ => ContentBlockView::Text { text: "[unsupported content block]".to_string() },
        }
    }
}

impl From<ContentBlockView> for oneai_core::ContentBlock {
    fn from(view: ContentBlockView) -> Self {
        match view {
            ContentBlockView::Text { text } => oneai_core::ContentBlock::Text { text },
            ContentBlockView::Image { mime_type, data } => oneai_core::ContentBlock::Image { mime_type, data },
            ContentBlockView::File { mime_type, uri } => oneai_core::ContentBlock::File { mime_type, uri },
            ContentBlockView::ToolCall { id, name, args_json } => {
                oneai_core::ContentBlock::ToolCall { id, name, args: args_json }
            },
            ContentBlockView::ToolResult { call_id, content } => {
                oneai_core::ContentBlock::ToolResult { call_id, content }
            },
            ContentBlockView::Thinking { text } => {
                oneai_core::ContentBlock::Thinking { text }
            },
        }
    }
}

// ─── ChatEventView ──────────────────────────────────────────────────

/// Streaming event surfaced to foreign code during `OneAISession::run_task`
/// (and `OneAiGroupChatSession::run_task`).
///
/// The foreign side implements `ChatEventCallback` and receives these events
/// in real time (callback-driven, not polled). Each variant maps from the
/// corresponding `AgentLoopObserver` callback in `oneai-agent`.
///
/// `speaker` identifies which agent produced the event. In single-agent
/// `run_task` it is always `None` (the foreign UI treats it as the single
/// assistant). In a group-chat session it carries the speaking member's id
/// so the UI can route the fragment to that member's bubble.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ChatEventView {
    /// A streamed text fragment from the model (typewriter effect).
    StreamChunk { text: String, speaker: Option<String> },
    /// A streamed thinking/reasoning fragment (extended-thinking models).
    Thinking { text: String, speaker: Option<String> },
    /// The model decided to call one or more tools (one event per call).
    ToolCall { id: String, name: String, args_json: String, speaker: Option<String> },
    /// A tool call finished with its result.
    ToolResult { call_id: String, tool_name: String, content: String, success: bool, speaker: Option<String> },
    /// The model produced a final direct answer (loop will end).
    DirectAnswer { text: String, speaker: Option<String> },
    /// The agent loop completed with the final answer.
    Complete { final_text: String, speaker: Option<String> },
    /// The agent loop errored out.
    Error { message: String, speaker: Option<String> },
}

// ─── ProviderConfigView ─────────────────────────────────────────────

/// Provider configuration for foreign-language App construction.
///
/// Passed to `OneAIAppBuilder::provider_config` so foreign code can build an
/// LLM-backed app without handling `Arc<dyn LlmProvider>` (which UniFFI can't
/// cross). `kind` selects the concrete provider constructed on the Rust side;
/// the remaining fields are forwarded to that provider's `ModelConfig`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ProviderConfigView {
    /// Provider kind: `"openai"`, `"anthropic"`, or `"ollama"`.
    pub kind: String,
    /// API key (required for openai/anthropic; ignored for ollama).
    pub api_key: Option<String>,
    /// Base URL override (OpenAI-compatible endpoints). `None` = provider default.
    pub base_url: Option<String>,
    /// Model name (e.g. `gpt-4o`, `claude-sonnet-4`, `llama3`).
    pub model: String,
    /// Ollama host (ollama only). `None` = `http://localhost`.
    pub host: Option<String>,
    /// Ollama port (ollama only). `None` = 11434.
    pub port: Option<u16>,
}

// ─── SessionInfoView ────────────────────────────────────────────────

/// Metadata about a saved conversation (UniFFI-compatible view).
///
/// Mirrors `oneai_core::SessionInfo` but with epoch-millis timestamps
/// (chrono `DateTime` can't cross UniFFI directly). Returned by
/// `OneAIApp::list_conversations()` so a foreign UI can render a session
/// list without loading full message histories.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionInfoView {
    /// The session/conversation ID.
    pub id: String,
    /// When the session was first created (epoch millis, UTC).
    pub created_at_ms: i64,
    /// When the session was last updated (epoch millis, UTC).
    pub updated_at_ms: i64,
    /// Number of messages in the conversation.
    pub message_count: u64,
    /// Short title from the first user message (whitespace-collapsed,
    /// truncated). `None` when the conversation has no user message yet.
    /// Render this as the drawer row label; fall back to a generic label.
    pub title: Option<String>,
}

impl From<oneai_core::SessionInfo> for SessionInfoView {
    fn from(s: oneai_core::SessionInfo) -> Self {
        Self {
            id: s.id,
            created_at_ms: s.created_at.timestamp_millis(),
            updated_at_ms: s.updated_at.timestamp_millis(),
            message_count: s.message_count as u64,
            title: s.title,
        }
    }
}

// ─── MessageView ────────────────────────────────────────────────────

/// A single conversation message (UniFFI-compatible view).
///
/// Flattened to `role` + `text` — multimodal content blocks are reduced to
/// their text content via `Message::text_content()`. Returned by
/// `OneAISession::messages()` so a foreign UI can replay a resumed
/// conversation's history. `role` is one of `"system"`, `"user"`,
/// `"assistant"`, `"tool"`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct MessageView {
    /// Author role: `"system"` / `"user"` / `"assistant"` / `"tool"`.
    pub role: String,
    /// Concatenated text content of the message's text blocks.
    pub text: String,
    /// Which agent produced this message. `None` for single-agent sessions
    /// (or system/tool messages); in a group-chat session it carries the
    /// speaking member's id, read from `Message.metadata["speaker"]`.
    pub speaker: Option<String>,
}

impl From<&oneai_core::Message> for MessageView {
    fn from(m: &oneai_core::Message) -> Self {
        let role = match m.role {
            oneai_core::Role::System => "system",
            oneai_core::Role::User => "user",
            oneai_core::Role::Assistant => "assistant",
            oneai_core::Role::Tool => "tool",
            _ => "system", // #[non_exhaustive] catch-all
        }.to_string();
        Self {
            role,
            text: m.text_content(),
            speaker: m.metadata.get("speaker").cloned(),
        }
    }
}

// ─── OneAIErrorView ─────────────────────────────────────────────────

/// Flat error view (UniFFI-compatible).
///
/// UniFFI requires errors to be simple enums with String payloads.
/// This flattens the nested OneAIError hierarchy into a single-level
/// enum suitable for cross-language error handling. Derives
/// `uniffi::Error` (not `uniffi::Enum`) so it can be used as the `E` in
/// `Result<T, E>` returns on exported methods.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
#[uniffi(flat_error)]
pub enum OneAIErrorView {
    /// LLM provider error.
    Provider { message: String },
    /// Output parser error.
    Parser { message: String },
    /// Tool execution error.
    Tool { message: String },
    /// Memory operation error.
    Memory { message: String },
    /// Workflow error.
    Workflow { message: String },
    /// Agent execution error.
    Agent { message: String },
    /// Skill selection error.
    Skill { message: String },
    /// Scheduler error.
    Scheduler { message: String },
    /// Persistence/checkpoint error.
    Persistence { message: String },
    /// RAG operation error.
    Rag { message: String },
    /// Configuration error.
    Config { message: String },
    /// Serialization error.
    Serialization { message: String },
    /// Network/HTTP error.
    Network { message: String },
    /// Timeout error.
    Timeout { message: String },
    /// Platform capability error.
    Platform { message: String },
    /// WASM sandbox error.
    Wasm { message: String },
    /// Generic error.
    Other { message: String },
}

impl From<oneai_core::OneAIError> for OneAIErrorView {
    fn from(err: oneai_core::OneAIError) -> Self {
        match err {
            oneai_core::OneAIError::Provider(msg) => OneAIErrorView::Provider { message: msg },
            oneai_core::OneAIError::Parser(parser_err) => OneAIErrorView::Parser {
                message: format!("{}", parser_err),
            },
            oneai_core::OneAIError::Tool(msg) => OneAIErrorView::Tool { message: msg },
            oneai_core::OneAIError::Memory(msg) => OneAIErrorView::Memory { message: msg },
            oneai_core::OneAIError::Workflow(msg) => OneAIErrorView::Workflow { message: msg },
            oneai_core::OneAIError::Agent(msg) => OneAIErrorView::Agent { message: msg },
            oneai_core::OneAIError::Skill(msg) => OneAIErrorView::Skill { message: msg },
            oneai_core::OneAIError::Scheduler(msg) => OneAIErrorView::Scheduler { message: msg },
            oneai_core::OneAIError::Persistence(msg) => OneAIErrorView::Persistence { message: msg },
            oneai_core::OneAIError::Rag(msg) => OneAIErrorView::Rag { message: msg },
            oneai_core::OneAIError::Config(msg) => OneAIErrorView::Config { message: msg },
            oneai_core::OneAIError::Serialization(msg) => OneAIErrorView::Serialization { message: msg },
            oneai_core::OneAIError::Network(msg) => OneAIErrorView::Network { message: msg },
            oneai_core::OneAIError::Timeout(msg) => OneAIErrorView::Timeout { message: msg },
            oneai_core::OneAIError::Platform(msg) => OneAIErrorView::Platform { message: msg },
            oneai_core::OneAIError::Wasm(msg) => OneAIErrorView::Wasm { message: msg },
            oneai_core::OneAIError::Other(msg) => OneAIErrorView::Other { message: msg },
            _ => OneAIErrorView::Other { message: "unknown error".to_string() }, // #[non_exhaustive] catch-all
        }
    }
}

// UniFFI's `Error` derive requires `Display` + `std::error::Error` so that
// `Result<T, OneAIErrorView>` can be lowered across the FFI boundary.
impl std::fmt::Display for OneAIErrorView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Each variant carries a `message: String`; surface a readable form.
        let (kind, msg) = match self {
            OneAIErrorView::Provider { message } => ("provider", message),
            OneAIErrorView::Parser { message } => ("parser", message),
            OneAIErrorView::Tool { message } => ("tool", message),
            OneAIErrorView::Memory { message } => ("memory", message),
            OneAIErrorView::Workflow { message } => ("workflow", message),
            OneAIErrorView::Agent { message } => ("agent", message),
            OneAIErrorView::Skill { message } => ("skill", message),
            OneAIErrorView::Scheduler { message } => ("scheduler", message),
            OneAIErrorView::Persistence { message } => ("persistence", message),
            OneAIErrorView::Rag { message } => ("rag", message),
            OneAIErrorView::Config { message } => ("config", message),
            OneAIErrorView::Serialization { message } => ("serialization", message),
            OneAIErrorView::Network { message } => ("network", message),
            OneAIErrorView::Timeout { message } => ("timeout", message),
            OneAIErrorView::Platform { message } => ("platform", message),
            OneAIErrorView::Wasm { message } => ("wasm", message),
            OneAIErrorView::Other { message } => ("other", message),
        };
        write!(f, "{} error: {}", kind, msg)
    }
}

impl std::error::Error for OneAIErrorView {}

// ─── PlatformView ───────────────────────────────────────────────────

/// Platform enum (UniFFI-compatible view).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum PlatformView {
    Macos,
    Windows,
    Linux,
    Android,
    Ios,
    Harmony,
    Unknown,
}

impl From<oneai_core::platform::Platform> for PlatformView {
    fn from(p: oneai_core::platform::Platform) -> Self {
        match p {
            oneai_core::platform::Platform::Macos => PlatformView::Macos,
            oneai_core::platform::Platform::Windows => PlatformView::Windows,
            oneai_core::platform::Platform::Linux => PlatformView::Linux,
            oneai_core::platform::Platform::Android => PlatformView::Android,
            oneai_core::platform::Platform::Ios => PlatformView::Ios,
            oneai_core::platform::Platform::Harmony => PlatformView::Harmony,
            oneai_core::platform::Platform::Unknown => PlatformView::Unknown,
            _ => PlatformView::Unknown, // #[non_exhaustive] catch-all
        }
    }
}

impl From<PlatformView> for oneai_core::platform::Platform {
    fn from(v: PlatformView) -> Self {
        match v {
            PlatformView::Macos => oneai_core::platform::Platform::Macos,
            PlatformView::Windows => oneai_core::platform::Platform::Windows,
            PlatformView::Linux => oneai_core::platform::Platform::Linux,
            PlatformView::Android => oneai_core::platform::Platform::Android,
            PlatformView::Ios => oneai_core::platform::Platform::Ios,
            PlatformView::Harmony => oneai_core::platform::Platform::Harmony,
            PlatformView::Unknown => oneai_core::platform::Platform::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_risk_level_conversion() {
        let core_level = oneai_core::RiskLevel::High;
        let view: RiskLevelView = core_level.into();
        assert_eq!(view, RiskLevelView::High);
        let back: oneai_core::RiskLevel = view.into();
        assert_eq!(back, oneai_core::RiskLevel::High);
    }

    #[test]
    fn test_approval_request_conversion() {
        let core_req = oneai_core::ApprovalRequest {
            tool_name: "shell".to_string(),
            args: serde_json::json!({"command": "ls"}),
            risk_level: oneai_core::RiskLevel::Medium,
            permission_level: None,
            justification: "List files".to_string(),
        };
        let view: ApprovalRequestView = core_req.clone().into();
        assert_eq!(view.tool_name, "shell");
        assert_eq!(view.args_json, "{\"command\":\"ls\"}");
        assert_eq!(view.risk_level, RiskLevelView::Medium);
        assert!(view.permission_level.is_none());
        let back: oneai_core::ApprovalRequest = view.into();
        assert_eq!(back.tool_name, "shell");
        assert_eq!(back.risk_level, oneai_core::RiskLevel::Medium);
        assert!(back.permission_level.is_none());
    }

    #[test]
    fn test_tool_output_conversion() {
        let output = oneai_core::ToolOutput {
            success: true,
            content: "42".to_string(),
            error: None,
        };
        let view: ToolOutputView = output.into();
        assert!(view.success);
        assert_eq!(view.content, "42");
        let back: oneai_core::ToolOutput = view.into();
        assert_eq!(back.content, "42");
    }

    #[test]
    fn test_content_block_conversion() {
        let text = oneai_core::ContentBlock::Text { text: "hello".to_string() };
        let view: ContentBlockView = text.into();
        assert!(matches!(view, ContentBlockView::Text { text: _ }));
    }

    #[test]
    fn test_error_conversion() {
        let core_err = oneai_core::OneAIError::Tool("not found".to_string());
        let view: OneAIErrorView = core_err.into();
        assert!(matches!(view, OneAIErrorView::Tool { message: _ }));
        if let OneAIErrorView::Tool { message } = view {
            assert!(message.contains("not found"));
        }
    }

    #[test]
    fn test_platform_conversion() {
        let core_platform = oneai_core::platform::Platform::Android;
        let view: PlatformView = core_platform.into();
        assert_eq!(view, PlatformView::Android);
        let back: oneai_core::platform::Platform = view.into();
        assert_eq!(back, oneai_core::platform::Platform::Android);
    }
}