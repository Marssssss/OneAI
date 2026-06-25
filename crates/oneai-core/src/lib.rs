//! # OneAI Core
//!
//! Core types, traits, and abstractions for the OneAI Agent framework.
//! New: budget management, PermissionLevel, platform capabilities, MemoryPersistence,
//! cost & usage management, rate limiting, circuit breaker, provider pool (fallback),
//! smart model router (cost/latency/quality routing), token counting & context management,
//! team coordination (multi-agent team strategies), handoff protocol (agent handoff-as-tool-call),
//! swarm orchestration (dynamic agent pools with capability-driven routing).

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.


pub mod types;
pub mod traits;
pub mod error;
pub mod platform;
pub mod budget;
pub mod platform_capabilities;
pub mod cost;
pub mod rate_limiter;
pub mod circuit_breaker;
pub mod provider_pool;
pub mod smart_router;
pub mod token_counter;
pub mod context_manager;
pub mod model_context;
pub mod context_accounting;
pub mod team;
pub mod handoff;
pub mod swarm;

pub use types::*;
pub use traits::*;
pub use error::*;
pub use platform::*;
pub use budget::*;
pub use platform_capabilities::*;
pub use cost::*;
pub use rate_limiter::*;
pub use circuit_breaker::*;
pub use provider_pool::*;
pub use smart_router::*;
pub use token_counter::*;
pub use context_manager::*;
pub use model_context::*;
pub use context_accounting::*;
pub use team::*;
pub use handoff::*;
pub use swarm::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_content_block_text() {
        let block = ContentBlock::Text { text: "Hello world".to_string() };
        let serialized = serde_json::to_string(&block).unwrap();
        assert!(serialized.contains("\"type\":\"text\""));
        assert!(serialized.contains("\"text\":\"Hello world\""));
    }

    #[test]
    fn test_content_block_tool_call() {
        let block = ContentBlock::ToolCall {
            id: "call_123".to_string(),
            name: "shell".to_string(),
            args: "{\"command\":\"ls\"}".to_string(),
        };
        let serialized = serde_json::to_string(&block).unwrap();
        assert!(serialized.contains("\"type\":\"tool_call\""));
        assert!(serialized.contains("\"id\":\"call_123\""));
    }

    #[test]
    fn test_content_block_roundtrip() {
        let blocks = vec![
            ContentBlock::Text { text: "Hello".to_string() },
            ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "tool_a".to_string(),
                args: "{}".to_string(),
            },
            ContentBlock::ToolResult {
                call_id: "call_1".to_string(),
                content: "result text".to_string(),
            },
        ];
        let json = serde_json::to_string(&blocks).unwrap();
        let parsed: Vec<ContentBlock> = serde_json::from_str(&json).unwrap();
        assert_eq!(blocks, parsed);
    }

    #[test]
    fn test_message_text_factory() {
        let msg = Message::user("Hello, AI!".to_string());
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.text_content(), "Hello, AI!");
        assert_eq!(msg.content.len(), 1);
    }

    #[test]
    fn test_message_tool_calls() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "I'll call a tool".to_string() },
                ContentBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "shell".to_string(),
                    args: "{\"cmd\":\"ls\"}".to_string(),
                },
                ContentBlock::ToolCall {
                    id: "call_2".to_string(),
                    name: "read_file".to_string(),
                    args: "{\"path\":\"/tmp/test\"}".to_string(),
                },
            ],
            metadata: HashMap::new(),
        };
        assert_eq!(msg.tool_calls().len(), 2);
    }

    #[test]
    fn test_conversation() {
        let mut conv = Conversation::new();
        conv.add_message(Message::system("You are helpful.".to_string()));
        conv.add_message(Message::user("What is Rust?".to_string()));
        assert_eq!(conv.len(), 2);
        assert!(!conv.is_empty());

        let last = conv.last_message();
        assert!(last.is_some());
        assert_eq!(last.unwrap().role, Role::User);
    }

    #[test]
    fn test_model_config_openai() {
        let config = ModelConfig::openai("sk-test".to_string(), "gpt-4".to_string());
        assert_eq!(config.provider_type, ProviderType::Cloud);
        assert_eq!(config.cloud_kind, Some(CloudProviderKind::OpenAI));
        assert_eq!(config.api_key, Some("sk-test".to_string()));
        assert_eq!(config.model_name, Some("gpt-4".to_string()));
    }

    #[test]
    fn test_model_config_anthropic() {
        let config = ModelConfig::anthropic("sk-ant-test".to_string(), "claude-sonnet-4-20250514".to_string());
        assert_eq!(config.provider_type, ProviderType::Cloud);
        assert_eq!(config.cloud_kind, Some(CloudProviderKind::Anthropic));
        assert_eq!(config.api_key, Some("sk-ant-test".to_string()));
    }

    #[test]
    fn test_model_config_ollama() {
        let config = ModelConfig::ollama("llama3".to_string());
        assert_eq!(config.provider_type, ProviderType::Local);
        assert_eq!(config.port, Some(11434));
        assert_eq!(config.model_name, Some("llama3".to_string()));
        assert_eq!(config.resolved_url(), "http://localhost:11434");
    }

    #[test]
    fn test_model_config_resolved_url_with_custom_port() {
        let config = ModelConfig::ollama_custom("http://192.168.1.100".to_string(), 8080, "llama3".to_string());
        assert_eq!(config.resolved_url(), "http://192.168.1.100:8080");
    }

    #[test]
    fn test_model_capability() {
        let cap = ModelCapability::claude_class();
        assert!(cap.supports_multimodal);
        assert!(cap.supports_streaming);
        assert!(cap.supports_tools);
        assert_eq!(cap.context_window_size, 200000);
    }

    #[test]
    fn test_risk_level() {
        assert_ne!(RiskLevel::Low, RiskLevel::High);
        assert_ne!(RiskLevel::Medium, RiskLevel::Low);
    }

    #[test]
    fn test_approval_response() {
        let approved = ApprovalResponse::Approved { modified_args: None };
        let json = serde_json::to_string(&approved).unwrap();
        assert!(json.contains("Approved"));
    }

    #[test]
    fn test_memory_entry_serialization() {
        let entry = MemoryEntry {
            id: "mem_1".to_string(),
            content: "User likes Rust".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: Some(vec![0.1, 0.2, 0.3]),
            metadata: HashMap::from([("source".to_string(), "conversation".to_string())]),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: MemoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.id, parsed.id);
        assert_eq!(entry.content, parsed.content);
    }

    #[test]
    fn test_skill_descriptor() {
        let skill = SkillDescriptor {
            name: "shell_executor".to_string(),
            description: "Execute shell commands".to_string(),
            prompt_template: "You can execute shell commands using the shell tool.".to_string(),
            trigger_keywords: vec!["shell".to_string(), "command".to_string(), "execute".to_string()],
            embedding: None,
        };
        let json = serde_json::to_string(&skill).unwrap();
        let parsed: SkillDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(skill.name, parsed.name);
    }

    #[test]
    fn test_global_state() {
        let state = GlobalState::new();
        assert!(state.conversation.is_empty());
        assert!(state.memory.is_empty());
        assert!(state.context.is_empty());
        assert!(state.step_results.is_empty());
    }

    #[test]
    fn test_reduction() {
        let reduction = Reduction::AppendMemory {
            entry: MemoryEntry {
                id: "mem_1".to_string(),
                content: "Result data".to_string(),
                timestamp: chrono::Utc::now(),
                embedding: None,
                metadata: HashMap::new(),
            },
        };
        let json = serde_json::to_string(&reduction).unwrap();
        let parsed: Reduction = serde_json::from_str(&json).unwrap();
        assert_eq!(reduction, parsed);
    }

    #[test]
    fn test_error_types() {
        let provider_err = OneAIError::Provider("API timeout".to_string());
        assert!(format!("{}", provider_err).contains("Provider error"));

        let parser_err = OneAIError::Parser(ParserError::FuzzyRepairFailed("bad json".to_string()));
        assert!(format!("{}", parser_err).contains("Fuzzy JSON repair failed"));
    }

    #[test]
    fn test_selection_mode() {
        assert_eq!(SelectionMode::KeywordMatch, SelectionMode::KeywordMatch);
        assert_ne!(SelectionMode::KeywordMatch, SelectionMode::VectorSimilarity);
    }

    #[test]
    fn test_platform_enum() {
        let platform = Platform::Android;
        let json = serde_json::to_string(&platform).unwrap();
        assert!(json.contains("android"));
    }

    #[test]
    fn test_content_block_image_roundtrip() {
        let block = ContentBlock::Image {
            mime_type: "image/png".to_string(),
            data: vec![1, 2, 3, 4],
        };
        let json = serde_json::to_string(&block).unwrap();
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, parsed);
    }
}