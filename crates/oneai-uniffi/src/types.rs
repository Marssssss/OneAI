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
    /// Justification for why the tool should be allowed.
    pub justification: String,
}

impl From<oneai_core::ApprovalRequest> for ApprovalRequestView {
    fn from(req: oneai_core::ApprovalRequest) -> Self {
        Self {
            tool_name: req.tool_name,
            args_json: req.args.to_string(),
            risk_level: RiskLevelView::from(req.risk_level),
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
            justification: view.justification,
        }
    }
}

// ─── ApprovalResponseView ───────────────────────────────────────────

/// User response to an approval request (UniFFI-compatible view).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ApprovalResponseView {
    /// Approved — allow execution (possibly with modified args).
    Approved {
        /// JSON-encoded modified arguments (null means no modification).
        modified_args_json: Option<String>,
    },
    /// Denied — block execution.
    Denied {
        reason: String,
    },
    /// Modified — allow execution with different arguments.
    Modified {
        /// JSON-encoded modified arguments.
        args_json: String,
    },
}

impl From<oneai_core::ApprovalResponse> for ApprovalResponseView {
    fn from(resp: oneai_core::ApprovalResponse) -> Self {
        match resp {
            oneai_core::ApprovalResponse::Approved { modified_args } => {
                ApprovalResponseView::Approved {
                    modified_args_json: modified_args.map(|v| v.to_string()),
                }
            }
            oneai_core::ApprovalResponse::Denied { reason } => {
                ApprovalResponseView::Denied { reason }
            }
            oneai_core::ApprovalResponse::Modified { args } => {
                ApprovalResponseView::Modified {
                    args_json: args.to_string(),
                }
            }
        }
    }
}

impl From<ApprovalResponseView> for oneai_core::ApprovalResponse {
    fn from(view: ApprovalResponseView) -> Self {
        match view {
            ApprovalResponseView::Approved { modified_args_json } => {
                oneai_core::ApprovalResponse::Approved {
                    modified_args: modified_args_json.map(|json| {
                        serde_json::from_str(&json).unwrap_or(serde_json::json!({}))
                    }),
                }
            }
            ApprovalResponseView::Denied { reason } => {
                oneai_core::ApprovalResponse::Denied { reason }
            }
            ApprovalResponseView::Modified { args_json } => {
                oneai_core::ApprovalResponse::Modified {
                    args: serde_json::from_str(&args_json)
                        .unwrap_or(serde_json::json!({})),
                }
            }
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
        }
    }
}

// ─── OneAIErrorView ─────────────────────────────────────────────────

/// Flat error view (UniFFI-compatible).
///
/// UniFFI requires errors to be simple enums with String payloads.
/// This flattens the nested OneAIError hierarchy into a single-level
/// enum suitable for cross-language error handling.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
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
    /// Approval gate error.
    Approval { message: String },
    /// Serialization error.
    Serialization { message: String },
    /// Network/HTTP error.
    Network { message: String },
    /// Timeout error.
    Timeout { message: String },
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
            oneai_core::OneAIError::Approval(approval_err) => OneAIErrorView::Approval {
                message: format!("{}", approval_err),
            },
            oneai_core::OneAIError::Serialization(msg) => OneAIErrorView::Serialization { message: msg },
            oneai_core::OneAIError::Network(msg) => OneAIErrorView::Network { message: msg },
            oneai_core::OneAIError::Timeout(msg) => OneAIErrorView::Timeout { message: msg },
            oneai_core::OneAIError::Other(msg) => OneAIErrorView::Other { message: msg },
        }
    }
}

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
            justification: "List files".to_string(),
        };
        let view: ApprovalRequestView = core_req.clone().into();
        assert_eq!(view.tool_name, "shell");
        assert_eq!(view.args_json, "{\"command\":\"ls\"}");
        assert_eq!(view.risk_level, RiskLevelView::Medium);
        let back: oneai_core::ApprovalRequest = view.into();
        assert_eq!(back.tool_name, "shell");
        assert_eq!(back.risk_level, oneai_core::RiskLevel::Medium);
    }

    #[test]
    fn test_approval_response_conversion() {
        let approved = oneai_core::ApprovalResponse::Approved { modified_args: None };
        let view: ApprovalResponseView = approved.into();
        assert!(matches!(view, ApprovalResponseView::Approved { modified_args_json: None }));

        let denied = oneai_core::ApprovalResponse::Denied { reason: "test".to_string() };
        let view: ApprovalResponseView = denied.into();
        assert!(matches!(view, ApprovalResponseView::Denied { reason: _ }));
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