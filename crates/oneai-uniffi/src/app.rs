//! UniFFI-exported App and Session wrappers for foreign-language bindings.

use std::sync::Arc;

use oneai_core::traits::Tool;
use oneai_agent::AgentLoop;

use crate::callback::{CallbackObserver, ChatEventCallback};
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
        let inner_session = self.inner.create_session();
        let session_id = inner_session.session_id().to_string();
        Arc::new(OneAISession {
            session_id,
            inner: tokio::sync::Mutex::new(inner_session),
            interrupt_slot: Arc::new(tokio::sync::Mutex::new(None)),
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
///
/// The inner `AppSession` is guarded by a `tokio::sync::Mutex` (not `std`) so
/// the guard can be held across `.await` points — `run_task` runs the full
/// agent loop, which is async and long-lived. `session_id` is cached as a
/// plain field so it stays a synchronous accessor.
#[derive(uniffi::Object)]
pub struct OneAISession {
    /// Cached session id (synchronous accessor, no lock needed).
    session_id: String,
    /// The wrapped AppSession. Locked with tokio's async mutex so the guard
    /// can survive the awaits inside `run_agent`.
    inner: tokio::sync::Mutex<oneai_app::AppSession>,
    /// Shared interrupt slot — `run_task` registers the running `AgentLoop`
    /// here so `interrupt()` can cancel it without re-locking the session.
    interrupt_slot: Arc<tokio::sync::Mutex<Option<AgentLoop>>>,
}

impl OneAISession {
    /// Get the session ID.
    pub fn session_id(&self) -> String {
        self.session_id.clone()
    }

    /// Send a user message.
    ///
    /// Note: this only appends the message to the conversation — it does NOT
    /// trigger inference. To get a model reply, call `run_task` afterwards (or
    /// instead). Kept for foreign code that wants to seed context manually.
    pub async fn send_user_message(&self, text: String) -> Result<(), OneAIErrorView> {
        let mut inner = self.inner.lock().await;
        inner.send_user_message(text).await
            .map_err(OneAIErrorView::from)
    }

    /// Execute a tool by name.
    pub async fn execute_tool(&self, name: String, args_json: String) -> Result<ToolOutputView, OneAIErrorView> {
        let inner = self.inner.lock().await;
        let args: serde_json::Value = serde_json::from_str(&args_json)
            .unwrap_or(serde_json::json!({}));
        inner.execute_tool(&name, args).await
            .map(ToolOutputView::from)
            .map_err(OneAIErrorView::from)
    }

    /// Retrieve relevant context from memory.
    pub async fn retrieve_memory(&self, query: String, top_k: u32) -> Result<String, OneAIErrorView> {
        let inner = self.inner.lock().await;
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
        let inner = self.inner.lock().await;
        inner.save_checkpoint().await
            .map_err(OneAIErrorView::from)
    }

    /// Run the agent loop for a task, streaming events to `callback`.
    ///
    /// This is the real inference entry point — `send_user_message` only seeds
    /// context; `run_task` actually drives the model, tools, and paradigms.
    /// Events (`StreamChunk`, `Thinking`, `ToolCall`, `ToolResult`,
    /// `DirectAnswer`, `Complete`) fire on the tokio worker thread; the foreign
    /// callback impl must marshal UI updates to the main thread.
    ///
    /// Returns `Ok` when the loop completes (the final answer is delivered as
    /// a `Complete` event), or an error view on failure.
    pub async fn run_task(
        &self,
        task: String,
        callback: Arc<dyn ChatEventCallback>,
    ) -> Result<(), OneAIErrorView> {
        let observer = CallbackObserver::new(callback);
        let mut inner = self.inner.lock().await;
        match inner.run_agent(&task, &observer, self.interrupt_slot.clone()).await {
            Ok(_result) => Ok(()),
            Err(e) => {
                // Surface the error both as a return value and as an event,
                // so a foreign UI that only listens to events still sees it.
                let view = OneAIErrorView::from(e);
                observer.emit(crate::types::ChatEventView::Error {
                    message: format!("{:?}", view),
                });
                Err(view)
            }
        }
    }

    /// Request the running agent loop (if any) to interrupt at the next
    /// iteration boundary. No-op if no `run_task` is in flight.
    pub async fn interrupt(&self) {
        let slot = self.interrupt_slot.lock().await;
        if let Some(loop_handle) = slot.as_ref() {
            loop_handle.request_interrupt(oneai_core::InterruptReason::Custom {
                reason: "Foreign interrupt() requested".to_string(),
            });
        }
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
            let inner = session.inner.lock().await;
            inner.memory_manager().archive_facts(vec![fact]).await;
        }

        let results = session.retrieve_memory("programming".to_string(), 5).await.unwrap();
        assert!(!results.is_empty());
        assert!(results.contains("Rust"));
    }

    // ─── S1: run_task + provider_config ──────────────────────────────

    /// Test callback that collects every event into a Mutex<Vec>.
    struct CollectingCallback {
        events: std::sync::Mutex<Vec<crate::types::ChatEventView>>,
    }

    impl crate::callback::ChatEventCallback for CollectingCallback {
        fn on_event(&self, event: crate::types::ChatEventView) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn test_session_run_task_emits_complete() {
        // Build an App with a MockProvider directly (provider_config would
        // construct a real network provider; for a unit test we inject the
        // mock via the underlying oneai_app::AppBuilder).
        let provider = Arc::new(oneai_agent::MockProvider::always_answers(
            "Hello from mock",
        ));
        let app_inner = oneai_app::AppBuilder::new()
            .provider(provider)
            .noop_interaction_gate()
            .default_parser()
            .build()
            .await
            .expect("build");
        let app = OneAIApp { inner: Arc::new(app_inner) };
        let session = app.create_session();

        let cb = Arc::new(CollectingCallback {
            events: std::sync::Mutex::new(Vec::new()),
        });
        session
            .run_task("Say hello".to_string(), cb.clone())
            .await
            .expect("run_task should complete");

        let events = cb.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| matches!(
                e,
                crate::types::ChatEventView::Complete { ref final_text }
                    if final_text.contains("Hello")
            )),
            "expected a Complete event containing 'Hello', got: {:?}",
            events
        );
    }

    #[tokio::test]
    async fn test_provider_config_sets_provider() {
        let builder = Arc::new(OneAIAppBuilder::new());
        let builder = builder
            .provider_config(crate::types::ProviderConfigView {
                kind: "openai".to_string(),
                api_key: Some("sk-test".to_string()),
                base_url: None,
                model: "gpt-4o".to_string(),
                host: None,
                port: None,
            })
            .expect("provider_config should accept openai");
        let app = builder.build().await.expect("build");
        assert!(
            app.has_provider(),
            "provider_config must wire a provider into the App"
        );
    }

    #[tokio::test]
    async fn test_provider_config_unknown_kind_errors() {
        let builder = Arc::new(OneAIAppBuilder::new());
        let res = builder.provider_config(crate::types::ProviderConfigView {
            kind: "bogus".to_string(),
            api_key: None,
            base_url: None,
            model: "x".to_string(),
            host: None,
            port: None,
        });
        assert!(
            res.is_err(),
            "unknown provider kind must return an error, not silently build"
        );
    }
}