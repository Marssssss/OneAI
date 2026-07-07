//! UniFFI-exported AppBuilder wrapper for foreign-language bindings.
//!
//! The `OneAIAppBuilder` wraps `oneai_app::AppBuilder` and provides
//! UniFFI-exportable methods that mirror the builder pattern.
//!
//! Since UniFFI methods on `Arc<Self>` consume self in a chained
//! builder pattern, we use a Mutex to allow the builder to be
//! taken out and reconstructed at each step.

use std::sync::Arc;

use oneai_memory::MemoryManager;
use oneai_persistence::FilePersistence;

use crate::types::{OneAIErrorView, ProviderConfigView};
use crate::app::OneAIApp;

/// UniFFI-exported AppBuilder wrapper.
///
/// Provides a builder-pattern API for foreign languages to construct
/// a OneAI App with all the necessary components.
#[derive(uniffi::Object)]
pub struct OneAIAppBuilder {
    inner: std::sync::Mutex<Option<oneai_app::AppBuilder>>,
}

impl OneAIAppBuilder {
    /// Create a new AppBuilder.
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Some(oneai_app::AppBuilder::new())),
        }
    }

    /// Use the no-op interaction gate (every point disabled, zero latency).
    pub fn noop_interaction_gate(self: Arc<Self>) -> Arc<Self> {
        let builder = self.take_inner().noop_interaction_gate();
        Arc::new(Self::from_builder(builder))
    }

    /// Use the deny-all interaction gate (every point aborted).
    pub fn deny_all_interaction_gate(self: Arc<Self>) -> Arc<Self> {
        let builder = self
            .take_inner()
            .interaction_gate(Arc::new(oneai_tool::DenyAllInteractionGate));
        Arc::new(Self::from_builder(builder))
    }

    /// Use the default 3-layer parser.
    pub fn default_parser(self: Arc<Self>) -> Arc<Self> {
        let builder = self.take_inner().default_parser();
        Arc::new(Self::from_builder(builder))
    }

    /// Set the LLM provider from a foreign-friendly config record.
    ///
    /// UniFFI can't cross `Arc<dyn LlmProvider>`, so foreign code passes a
    /// `ProviderConfigView` and the concrete provider is constructed on the
    /// Rust side (mirroring `ProviderFactory` in `lib.rs`). `kind` selects the
    /// provider: `"openai"` (OpenAI-compatible), `"anthropic"`, or `"ollama"`.
    /// Returns an error view for an unknown `kind`.
    pub fn provider_config(
        self: Arc<Self>,
        cfg: ProviderConfigView,
    ) -> std::result::Result<Arc<Self>, OneAIErrorView> {
        let provider: Arc<dyn oneai_core::traits::LlmProvider> = match cfg.kind.as_str() {
            "openai" => {
                let config = oneai_core::ModelConfig::openai_compatible(
                    cfg.api_key.unwrap_or_default(),
                    cfg.base_url
                        .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
                    cfg.model,
                );
                Arc::new(oneai_provider::OpenAIProvider::new(config))
            }
            "anthropic" => {
                let config =
                    oneai_core::ModelConfig::anthropic(cfg.api_key.unwrap_or_default(), cfg.model);
                Arc::new(oneai_provider::AnthropicProvider::new(config))
            }
            "ollama" => {
                let config = if let Some(host) = cfg.host {
                    oneai_core::ModelConfig::ollama_custom(
                        host,
                        cfg.port.unwrap_or(11434),
                        cfg.model,
                    )
                } else {
                    oneai_core::ModelConfig::ollama(cfg.model)
                };
                Arc::new(oneai_provider::OllamaProvider::new(config))
            }
            other => {
                return Err(OneAIErrorView::Config {
                    message: format!(
                        "Unknown provider kind '{}'; expected openai/anthropic/ollama",
                        other
                    ),
                });
            }
        };
        let builder = self.take_inner().provider(provider);
        Ok(Arc::new(Self::from_builder(builder)))
    }

    /// Set the memory manager with custom config.
    pub fn memory_manager_with_config(self: Arc<Self>, threshold_tokens: u32) -> Arc<Self> {
        let config = oneai_memory::MemoryManagerConfig {
            compression_threshold_tokens: threshold_tokens as usize,
            ..Default::default()
        };
        let manager = Arc::new(MemoryManager::with_config(config));
        let builder = self.take_inner().memory_manager(manager);
        Arc::new(Self::from_builder(builder))
    }

    /// Set the persistence layer.
    pub fn persistence(self: Arc<Self>, path: String) -> Arc<Self> {
        let persistence = Arc::new(FilePersistence::new(&path));
        let builder = self.take_inner().persistence(persistence);
        Arc::new(Self::from_builder(builder))
    }

    /// Build the application.
    ///
    /// This is async because domain pack tools are eagerly registered
    /// at build time, which requires async tool registry operations.
    pub async fn build(self: Arc<Self>) -> Result<Arc<OneAIApp>, OneAIErrorView> {
        let builder = self.take_inner();
        builder.build()
            .await
            .map(|app| Arc::new(OneAIApp { inner: Arc::new(app) }))
            .map_err(OneAIErrorView::from)
    }
}

impl OneAIAppBuilder {
    /// Take the inner builder out of the mutex.
    fn take_inner(&self) -> oneai_app::AppBuilder {
        self.inner.lock().unwrap().take().unwrap_or_else(oneai_app::AppBuilder::new)
    }

    /// Create from a raw builder.
    fn from_builder(builder: oneai_app::AppBuilder) -> Self {
        Self {
            inner: std::sync::Mutex::new(Some(builder)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_app_builder_auto_approve() {
        let builder = Arc::new(OneAIAppBuilder::new());
        let builder = builder.noop_interaction_gate();
        let builder = builder.default_parser();
        let app = builder.build().await.expect("Build should succeed");

        assert!(!app.inner.has_provider());
    }

    #[tokio::test]
    async fn test_app_builder_blocking() {
        let builder = Arc::new(OneAIAppBuilder::new());
        let builder = builder.deny_all_interaction_gate();
        let builder = builder.default_parser();
        let app = builder.build().await.expect("Build should succeed");

        app.inner.register_tool(Arc::new(oneai_tool::ShellTool::new())).await.unwrap();

        let session = app.inner.create_session();
        let result = session.execute_tool("shell", serde_json::json!({"command": "echo test"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_app_builder_with_persistence() {
        let builder = Arc::new(OneAIAppBuilder::new());
        let builder = builder.noop_interaction_gate();
        let builder = builder.persistence("/tmp/oneai_uniffi_test".to_string());
        let app = builder.build().await.expect("Build should succeed");

        let session = app.inner.create_session();
        let checkpoint_id = session.save_checkpoint().await.unwrap();
        assert!(!checkpoint_id.is_empty());
    }
}