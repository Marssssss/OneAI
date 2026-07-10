//! # OneAI UniFFI
//!
//! UniFFI binding definitions for cross-platform foreign-language interfaces.
//! Generates Kotlin, Swift, C++, and C# bindings from Rust types.
//!
//! Currently, UniFFI doesn't support trait objects (`dyn LlmProvider`, etc.)
//! directly. We export concrete types that can be used from foreign languages,
//! and provide factory methods for creating them.
//!
//! The binding strategy:
//! - View types (RiskLevelView, ApprovalRequestView, etc.) get UniFFI derive macros
//! - Traits are exposed as Rust-only (foreign code uses concrete implementations)
//! - Factory methods create pre-configured concrete instances
//! - App/AppBuilder wrappers provide idiomatic foreign-language APIs

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


// Re-export all OneAI crates
pub use oneai_core;
pub use oneai_provider;
pub use oneai_agent;
pub use oneai_tool;
pub use oneai_memory;
pub use oneai_skill;
pub use oneai_parser;
pub use oneai_workflow;
pub use oneai_scheduler;
pub use oneai_persistence;
pub use oneai_rag;
pub use oneai_app;

// UniFFI view types (derive uniffi::Record/uniffi::Enum)
pub mod types;
pub use types::*;

// Foreign-implemented callback interfaces + Rust-side observer adapter.
pub mod callback;
pub use callback::*;

// UniFFI-exported wrapper objects
pub mod app_builder;
pub mod app;

// `extern "C"` JSON facade for runtimes UniFFI 0.32 can't generate bindings
// for (C# / NAPI-C++). Exported from the same cdylib alongside the uniffi
// symbols. See c_facade.rs.
pub mod c_facade;

// Register all UniFFI-exported types with the scaffolding system
// (required for derive-macro-only approach, no UDL file)
uniffi::setup_scaffolding!("oneai");

// ─── Factory Types (UniFFI-exported) ───────────────────────────────

/// A concrete LLM provider factory for foreign-language bindings.
///
/// Since UniFFI can't handle `Arc<dyn LlmProvider>`, this factory
/// creates concrete provider instances based on configuration.
#[derive(uniffi::Object)]
pub struct ProviderFactory;

// NOTE: ProviderFactory/MemoryFactory/ToolFactory are Rust-only helpers —
// their methods return concrete internal types (OpenAIProvider, ShellTool,
// MemoryManager) that are NOT UniFFI-exportable, so the impls are not
// `#[uniffi::export]`-ed. Foreign code sets providers via
// `OneAIAppBuilder::provider_config(ProviderConfigView)` instead.
impl ProviderFactory {
    /// Create an OpenAI-compatible provider.
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self
    }

    /// Create an OpenAI-compatible provider.
    #[uniffi::method]
    pub fn create_openai(
        &self,
        api_key: String,
        base_url: Option<String>,
        model_name: String,
    ) -> oneai_provider::OpenAIProvider {
        let config = oneai_core::ModelConfig::openai_compatible(
            api_key,
            base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            model_name,
        );
        oneai_provider::OpenAIProvider::new(config)
    }

    /// Create an Anthropic Claude provider.
    #[uniffi::method]
    pub fn create_anthropic(
        &self,
        api_key: String,
        model_name: String,
    ) -> oneai_provider::AnthropicProvider {
        let config = oneai_core::ModelConfig::anthropic(api_key, model_name);
        oneai_provider::AnthropicProvider::new(config)
    }

    /// Create an Ollama local provider.
    #[uniffi::method]
    pub fn create_ollama(
        &self,
        host: Option<String>,
        port: Option<u16>,
        model_name: String,
    ) -> oneai_provider::OllamaProvider {
        let config = if let Some(h) = host {
            oneai_core::ModelConfig::ollama_custom(h, port.unwrap_or(11434), model_name)
        } else {
            oneai_core::ModelConfig::ollama(model_name)
        };
        oneai_provider::OllamaProvider::new(config)
    }
}

/// A concrete memory manager factory for foreign-language bindings.
#[derive(uniffi::Object)]
pub struct MemoryFactory;

impl MemoryFactory {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self
    }

    /// Create a default memory manager (STM window size 20, no compression).
    #[uniffi::method]
    pub fn create_memory_manager(&self) -> oneai_memory::MemoryManager {
        oneai_memory::MemoryManager::new()
    }

    /// Create a memory manager with a custom compression threshold.
    #[uniffi::method]
    pub fn create_memory_manager_with_config(
        &self,
        threshold_tokens: u32,
    ) -> oneai_memory::MemoryManager {
        let config = oneai_memory::MemoryManagerConfig {
            compression_threshold_tokens: threshold_tokens as usize,
            ..Default::default()
        };
        oneai_memory::MemoryManager::with_config(config)
    }
}

/// A concrete tool factory for foreign-language bindings.
#[derive(uniffi::Object)]
pub struct ToolFactory;

impl ToolFactory {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self
    }

    /// Create a calculator tool.
    #[uniffi::method]
    pub fn create_calculator(&self) -> oneai_tool::CalculatorTool {
        oneai_tool::CalculatorTool::new()
    }

    /// Create a file read tool.
    #[uniffi::method]
    pub fn create_file_reader(&self, max_size_bytes: Option<u64>) -> oneai_tool::FileReadTool {
        match max_size_bytes {
            Some(size) => oneai_tool::FileReadTool::with_max_size(size as usize),
            None => oneai_tool::FileReadTool::new(),
        }
    }

    /// Create a file write tool.
    #[uniffi::method]
    pub fn create_file_writer(&self) -> oneai_tool::FileWriteTool {
        oneai_tool::FileWriteTool::new()
    }

    /// Create a shell tool with default timeout.
    #[uniffi::method]
    pub fn create_shell(&self) -> oneai_tool::ShellTool {
        oneai_tool::ShellTool::new()
    }

    /// Create a shell tool with custom timeout.
    #[uniffi::method]
    pub fn create_shell_with_timeout(&self, timeout_secs: u64) -> oneai_tool::ShellTool {
        oneai_tool::ShellTool::with_timeout(timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::LlmProvider;

    #[test]
    fn test_provider_factory_openai() {
        let factory = ProviderFactory::new();
        let provider = factory.create_openai(
            "test_key".to_string(),
            None,
            "gpt-4".to_string(),
        );
        assert_eq!(provider.config().model_name.clone().unwrap(), "gpt-4");
    }

    #[test]
    fn test_provider_factory_anthropic() {
        let factory = ProviderFactory::new();
        let provider = factory.create_anthropic(
            "test_key".to_string(),
            "claude-3-opus".to_string(),
        );
        assert_eq!(provider.config().model_name.clone().unwrap(), "claude-3-opus");
    }

    #[test]
    fn test_provider_factory_ollama() {
        let factory = ProviderFactory::new();
        let provider = factory.create_ollama(
            None,
            None,
            "llama3".to_string(),
        );
        assert_eq!(provider.config().model_name.clone().unwrap(), "llama3");
    }

    #[test]
    fn test_memory_factory() {
        let factory = MemoryFactory::new();
        let _default = factory.create_memory_manager();
        let _custom = factory.create_memory_manager_with_config(2000);
    }

    #[test]
    fn test_tool_factory() {
        let factory = ToolFactory::new();
        let _calc = factory.create_calculator();
        let _reader = factory.create_file_reader(None);
        let _reader_custom = factory.create_file_reader(Some(2048));
        let _writer = factory.create_file_writer();
        let _shell = factory.create_shell();
        let _shell_custom = factory.create_shell_with_timeout(60);
    }
}