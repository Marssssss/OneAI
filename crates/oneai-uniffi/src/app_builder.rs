//! UniFFI-exported AppBuilder wrapper for foreign-language bindings.
//!
//! The `OneAIAppBuilder` wraps `oneai_app::AppBuilder` and provides
//! UniFFI-exportable methods that mirror the builder pattern.
//!
//! Since UniFFI methods on `Arc<Self>` consume self in a chained
//! builder pattern, we use a Mutex to allow the builder to be
//! taken out and reconstructed at each step.

use std::sync::Arc;

use oneai_core::traits::Tool;
use oneai_memory::MemoryManager;
use oneai_persistence::FilePersistence;

use crate::types::{OneAIErrorView, ProviderConfigView, EmbeddingConfigView};
use crate::app::OneAIApp;

/// UniFFI-exported AppBuilder wrapper.
///
/// Provides a builder-pattern API for foreign languages to construct
/// a OneAI App with all the necessary components.
///
/// `extra_tools` survives the consumed-`Arc` builder chain (threaded
/// through `from_builder`) so `default_tools()` set before
/// `provider_config()` etc. is not lost.
#[derive(uniffi::Object)]
pub struct OneAIAppBuilder {
    inner: std::sync::Mutex<Option<oneai_app::AppBuilder>>,
    extra_tools: std::sync::Mutex<Vec<Arc<dyn Tool>>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl OneAIAppBuilder {
    /// Create a new AppBuilder.
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Some(oneai_app::AppBuilder::new())),
            extra_tools: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Use the no-op interaction gate (every point disabled, zero latency).
    #[uniffi::method]
    pub fn noop_interaction_gate(self: Arc<Self>) -> Arc<Self> {
        let extras = self.take_extra_tools();
        let builder = self.take_inner().noop_interaction_gate();
        Arc::new(Self::from_builder(builder, extras))
    }

    /// Use the deny-all interaction gate (every point aborted).
    #[uniffi::method]
    pub fn deny_all_interaction_gate(self: Arc<Self>) -> Arc<Self> {
        let extras = self.take_extra_tools();
        let builder = self
            .take_inner()
            .interaction_gate(Arc::new(oneai_tool::DenyAllInteractionGate));
        Arc::new(Self::from_builder(builder, extras))
    }

    /// Use the default 3-layer parser.
    #[uniffi::method]
    pub fn default_parser(self: Arc<Self>) -> Arc<Self> {
        let extras = self.take_extra_tools();
        let builder = self.take_inner().default_parser();
        Arc::new(Self::from_builder(builder, extras))
    }

    /// Register the built-in read-only research tools (`web_search` +
    /// `web_fetch`) so a foreign app gets a working agent without wiring a
    /// DomainPack. `web_search` defaults to the DuckDuckGo backend — no API
    /// key required (set `ONEAI_SEARCH_*` env for Google/Bing/SerpAPI, though
    /// env is typically unavailable on mobile). Idempotent.
    #[uniffi::method]
    pub fn default_tools(self: Arc<Self>) -> Arc<Self> {
        let extras = self.take_extra_tools();
        let builder = self.take_inner();
        let mut extras = extras;
        // Web access.
        if !extras.iter().any(|t| t.name() == "web_search") {
            extras.push(Arc::new(oneai_tool::WebSearchTool::new()));
        }
        if !extras.iter().any(|t| t.name() == "web_fetch") {
            extras.push(Arc::new(oneai_tool::WebFetchTool::new()));
        }
        // File access — so an agent (incl. group-chat persona members, whose
        // sub-agent factory is None and therefore cannot `delegate` a write to
        // a Code sub-agent) can read/write local files directly when asked
        // (e.g. "把面试总结写到本地文件"). Without these the model's only
        // path to file I/O is the `delegate` meta-tool, which hangs.
        if !extras.iter().any(|t| t.name() == "read_file") {
            extras.push(Arc::new(oneai_tool::FileReadTool::new()));
        }
        if !extras.iter().any(|t| t.name() == "write_file") {
            extras.push(Arc::new(oneai_tool::FileWriteTool::new()));
        }
        Arc::new(Self::from_builder(builder, extras))
    }

    /// Set the LLM provider from a foreign-friendly config record.
    ///
    /// UniFFI can't cross `Arc<dyn LlmProvider>`, so foreign code passes a
    /// `ProviderConfigView` and the concrete provider is constructed on the
    /// Rust side (mirroring `ProviderFactory` in `lib.rs`). `kind` selects the
    /// provider: `"openai"` (OpenAI-compatible), `"anthropic"`, or `"ollama"`.
    /// Returns an error view for an unknown `kind`.
    #[uniffi::method]
    pub fn provider_config(
        self: Arc<Self>,
        cfg: ProviderConfigView,
    ) -> std::result::Result<Arc<Self>, OneAIErrorView> {
        let extras = self.take_extra_tools();
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
        Ok(Arc::new(Self::from_builder(builder, extras)))
    }

    /// Set the embedding provider from a foreign-friendly config record.
    ///
    /// Foreign platforms normally skip this for zero-config auto-detection;
    /// call it only when the user explicitly chose a provider/key in settings.
    /// `provider = "auto"` resolves at build time (env keys / local Ollama;
    /// nothing available → `None`, memory recall falls back to keyword matching).
    #[uniffi::method]
    pub fn embedding_config(
        self: Arc<Self>,
        cfg: EmbeddingConfigView,
    ) -> std::result::Result<Arc<Self>, OneAIErrorView> {
        let extras = self.take_extra_tools();
        let builder = self.take_inner().embedding_config(cfg.to_engine());
        Ok(Arc::new(Self::from_builder(builder, extras)))
    }

    /// Set the agent's base system prompt (persona / role instruction).
    ///
    /// This is the foreign-friendly way to give a single-agent app a persona
    /// without constructing a full `DomainPack`. It injects a lightweight
    /// `DomainPack` whose only non-default field is `system_prompt_template`,
    /// via the same `AppBuilder::domain_pack` path the engine uses. Call
    /// before `provider_config`/`build`; the latest call wins. Idempotent in
    /// the sense that each call replaces the pack (no accumulation).
    #[uniffi::method]
    pub fn system_prompt(self: Arc<Self>, prompt: String) -> Arc<Self> {
        let extras = self.take_extra_tools();
        let pack = oneai_domain::DomainPackBuilder::new("persona")
            .description("Foreign-configured persona system prompt.")
            .system_prompt(prompt)
            .build();
        let builder = self.take_inner().domain_pack(pack);
        Arc::new(Self::from_builder(builder, extras))
    }

    /// Set the memory manager with custom config.
    #[uniffi::method]
    pub fn memory_manager_with_config(self: Arc<Self>, threshold_tokens: u32) -> Arc<Self> {
        let extras = self.take_extra_tools();
        let config = oneai_memory::MemoryManagerConfig {
            compression_threshold_tokens: threshold_tokens as usize,
            ..Default::default()
        };
        let manager = Arc::new(MemoryManager::with_config(config));
        let builder = self.take_inner().memory_manager(manager);
        Arc::new(Self::from_builder(builder, extras))
    }

    /// Set the persistence layer.
    #[uniffi::method]
    pub fn persistence(self: Arc<Self>, path: String) -> Arc<Self> {
        let extras = self.take_extra_tools();
        let persistence = Arc::new(FilePersistence::new(&path));
        let builder = self.take_inner().persistence(persistence);
        Arc::new(Self::from_builder(builder, extras))
    }

    /// Enable SQLite persistence at a foreign-provided db path.
    ///
    /// This is the mobile-friendly counterpart to `AppBuilder::sqlite_persistence_at`
    /// — on Android/iOS the app's private files dir is the only reliably writable
    /// location, and the default `~/.oneai/oneai.db` path does not exist. Enables
    /// conversation save/load (multi-session resume), STM/LTM, and checkpoint
    /// persistence. `run_task` auto-saves the conversation after each agent run,
    /// so no explicit save call is needed from foreign code.
    #[uniffi::method]
    pub fn sqlite_persistence_at(self: Arc<Self>, path: String) -> Arc<Self> {
        let extras = self.take_extra_tools();
        let builder = self.take_inner().sqlite_persistence_at(&path);
        Arc::new(Self::from_builder(builder, extras))
    }

    /// Build the application.
    ///
    /// This is async because domain pack tools are eagerly registered
    /// at build time, which requires async tool registry operations.
    /// `extra_tools` (e.g. from `default_tools()`) are registered on the
    /// built App here.
    #[uniffi::method]
    pub async fn build(self: Arc<Self>) -> Result<Arc<OneAIApp>, OneAIErrorView> {
        let extras = self.take_extra_tools();
        let builder = self.take_inner();
        let app = builder
            .build()
            .await
            .map(|app| Arc::new(OneAIApp { inner: Arc::new(app) }))
            .map_err(OneAIErrorView::from)?;
        for tool in extras {
            if let Err(e) = app.inner.register_tool(tool).await {
                eprintln!("default tool registration failed: {:?}", e);
            }
        }
        Ok(app)
    }
}

impl OneAIAppBuilder {
    /// Take the inner builder out of the mutex.
    fn take_inner(&self) -> oneai_app::AppBuilder {
        self.inner.lock().unwrap().take().unwrap_or_else(oneai_app::AppBuilder::new)
    }

    /// Take the extra tools (web_search/web_fetch from `default_tools`) out
    /// of the mutex so they survive the consumed-`Arc` builder chain.
    fn take_extra_tools(&self) -> Vec<Arc<dyn Tool>> {
        std::mem::take(&mut *self.extra_tools.lock().unwrap())
    }

    /// Create from a raw builder, carrying over any extra tools.
    fn from_builder(builder: oneai_app::AppBuilder, extras: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            inner: std::sync::Mutex::new(Some(builder)),
            extra_tools: std::sync::Mutex::new(extras),
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

        // Persistence wires a StatePersistence layer at the App level; the
        // per-session durable substrate is now the working-state event log,
        // not full-state checkpoints. Just verify the session builds.
        let _session = app.inner.create_session();
    }
}