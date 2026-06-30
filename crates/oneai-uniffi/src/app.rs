//! UniFFI-exported App and Session wrappers for foreign-language bindings.

use std::sync::Arc;

use oneai_core::traits::Tool;

use crate::types::{ToolOutputView, OneAIErrorView, PlatformView};

/// UniFFI-exported App wrapper.
///
/// Provides methods for creating sessions and registering tools
/// that can be called from foreign languages.
#[derive(uniffi::Object)]
pub struct OneAIApp {
    pub(crate) inner: Arc<oneai_app::App>,
}

impl OneAIApp {
    /// Create a new agent session.
    pub fn create_session(&self) -> Arc<OneAISession> {
        Arc::new(OneAISession {
            inner: std::sync::Mutex::new(self.inner.create_session()),
        })
    }

    /// Register a tool.
    pub async fn register_tool(&self, tool: Arc<OneAIToolWrapper>) -> Result<(), OneAIErrorView> {
        self.inner.register_tool(tool.inner.clone()).await
            .map_err(OneAIErrorView::from)
    }

    /// Check if a provider is configured.
    pub fn has_provider(&self) -> bool {
        self.inner.has_provider()
    }

    /// Get the current platform.
    pub fn platform(&self) -> PlatformView {
        PlatformView::from(*self.inner.platform())
    }
}

/// UniFFI-exported Session wrapper.
///
/// Provides methods for sending messages, executing tools,
/// retrieving memory, and saving checkpoints.
#[derive(uniffi::Object)]
pub struct OneAISession {
    inner: std::sync::Mutex<oneai_app::AppSession>,
}

impl OneAISession {
    /// Get the session ID.
    pub fn session_id(&self) -> String {
        self.inner.lock().unwrap().session_id().to_string()
    }

    /// Send a user message.
    pub async fn send_user_message(&self, text: String) -> Result<(), OneAIErrorView> {
        let mut inner = self.inner.lock().unwrap();
        inner.send_user_message(text).await
            .map_err(OneAIErrorView::from)
    }

    /// Execute a tool by name.
    pub async fn execute_tool(&self, name: String, args_json: String) -> Result<ToolOutputView, OneAIErrorView> {
        let inner = self.inner.lock().unwrap();
        let args: serde_json::Value = serde_json::from_str(&args_json)
            .unwrap_or(serde_json::json!({}));
        inner.execute_tool(&name, args).await
            .map(ToolOutputView::from)
            .map_err(OneAIErrorView::from)
    }

    /// Retrieve relevant context from memory.
    pub async fn retrieve_memory(&self, query: String, top_k: u32) -> Result<String, OneAIErrorView> {
        let inner = self.inner.lock().unwrap();
        inner.retrieve_memory(&query, top_k as usize).await
            .map(|entries| {
                entries.iter()
                    .map(|e| e.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .map_err(OneAIErrorView::from)
    }

    /// Save a checkpoint.
    pub async fn save_checkpoint(&self) -> Result<String, OneAIErrorView> {
        let inner = self.inner.lock().unwrap();
        inner.save_checkpoint().await
            .map_err(OneAIErrorView::from)
    }
}

// ─── OneAIToolWrapper ──────────────────────────────────────────────

/// UniFFI-exported tool wrapper.
///
/// Wraps `Arc<dyn Tool>` in a concrete UniFFI-exportable type.
/// Created by `ToolFactory` methods.
#[derive(uniffi::Object)]
pub struct OneAIToolWrapper {
    pub(crate) inner: Arc<dyn Tool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_builder::OneAIAppBuilder;

    #[tokio::test]
    async fn test_app_create_session() {
        let builder = Arc::new(OneAIAppBuilder::new());
        let builder = builder.noop_interaction_gate();
        let app = builder.build().await.expect("Build should succeed");

        let session = app.create_session();
        assert!(!session.session_id().is_empty());
    }

    #[tokio::test]
    async fn test_session_send_message() {
        let builder = Arc::new(OneAIAppBuilder::new());
        let builder = builder.noop_interaction_gate();
        let app = builder.build().await.expect("Build should succeed");

        let session = app.create_session();
        session.send_user_message("Hello from UniFFI!".to_string()).await.unwrap();
    }

    #[tokio::test]
    async fn test_session_execute_tool() {
        let builder = Arc::new(OneAIAppBuilder::new());
        let builder = builder.noop_interaction_gate();
        let app = builder.build().await.expect("Build should succeed");

        let calc_wrapper = Arc::new(OneAIToolWrapper {
            inner: Arc::new(oneai_tool::CalculatorTool::new()),
        });
        app.register_tool(calc_wrapper).await.unwrap();

        let session = app.create_session();
        let result = session.execute_tool("calculator".to_string(), "{\"expression\":\"2+3\"}".to_string()).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "5");
    }

    #[tokio::test]
    async fn test_session_retrieve_memory() {
        let builder = Arc::new(OneAIAppBuilder::new());
        let builder = builder.noop_interaction_gate();
        let app = builder.build().await.expect("Build should succeed");

        let session = app.create_session();

        // Working memory is single-sourced on the Conversation (M1); the
        // canonical long-term memory is the fact_archive. Insert a fact and
        // verify retrieve_memory recalls it (recall_facts → fact_archive).
        let fact = oneai_core::MemoryFact {
            id: "f1".to_string(),
            user_id: String::new(),
            session_id: String::new(),
            fact_type: oneai_core::FactType::new("decision"),
            subject: "lang".to_string(),
            predicate: "is".to_string(),
            content: "Rust is a programming language".to_string(),
            embedding: None,
            metadata: std::collections::HashMap::new(),
            importance: 0.5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        };
        {
            let inner = session.inner.lock().unwrap();
            inner.memory_manager().archive_facts(vec![fact]).await;
        }

        let results = session.retrieve_memory("programming".to_string(), 5).await.unwrap();
        assert!(!results.is_empty());
        assert!(results.contains("Rust"));
    }
}