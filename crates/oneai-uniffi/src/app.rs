//! UniFFI-exported App and Session wrappers for foreign-language bindings.

use std::sync::Arc;

use futures::FutureExt; // `catch_unwind` for the long-running FFI entries

use oneai_agent::AgentLoop;
use oneai_core::traits::Tool;

use crate::callback::{CallbackObserver, ChatEventCallback};
use crate::group_chat::{OneAiGroupChatSession, ScenarioSpecView};
use crate::types::{
    MessageView, OneAIErrorView, PlatformView, SessionInfoView, ToolOutputView, TranscriptPage,
};

/// Map a memory-layer transcript page to the UniFFI view (stringified cursor).
fn transcript_page_view(page: oneai_memory::TranscriptPageData) -> TranscriptPage {
    TranscriptPage {
        messages: page.messages.iter().map(MessageView::from).collect(),
        older_cursor: page.older_cursor.map(|r| r.to_string()),
        total: page.total as u32,
    }
}

/// UniFFI-exported App wrapper.
///
/// Provides methods for creating sessions and registering tools
/// that can be called from foreign languages.
#[derive(uniffi::Object)]
pub struct OneAIApp {
    pub(crate) inner: Arc<oneai_app::App>,
}

#[uniffi::export(async_runtime = "tokio")]
impl OneAIApp {
    /// Create a new agent session.
    #[uniffi::method]
    pub fn create_session(&self) -> Arc<OneAISession> {
        let inner_session = self.inner.create_session();
        let session_id = inner_session.session_id().to_string();
        tracing::info!("create_session (new uuid) id={}", session_id);
        Arc::new(OneAISession {
            session_id,
            inner: tokio::sync::Mutex::new(inner_session),
            interrupt_slot: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// Create or resume a session bound to an existing conversation id.
    ///
    /// If SQLite persistence is enabled (`sqlite_persistence_at`) and a
    /// conversation with this id is saved, its history is loaded into the new
    /// session. Use `messages()` afterwards to replay it in the UI. For an
    /// unknown id, an empty conversation is created (a brand-new chat) and will
    /// be auto-saved under this id on the next `run_task`.
    #[uniffi::method]
    pub async fn create_session_with_id(&self, id: String) -> Arc<OneAISession> {
        let inner_session = self.inner.create_session_with_id(&id).await;
        let session_id = inner_session.session_id().to_string();
        tracing::info!(
            "create_session_with_id (resume) requested_id={} resolved_id={}",
            id,
            session_id
        );
        Arc::new(OneAISession {
            session_id,
            inner: tokio::sync::Mutex::new(inner_session),
            interrupt_slot: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// List all saved conversations (metadata only). Empty when SQLite
    /// persistence is not enabled. Order is implementation-defined (currently
    /// newest-first by row id); sort on the foreign side if a specific order
    /// is needed.
    #[uniffi::method]
    pub async fn list_conversations(&self) -> Vec<SessionInfoView> {
        self.inner
            .list_conversations()
            .await
            .into_iter()
            .map(SessionInfoView::from)
            .collect()
    }

    /// Delete a saved conversation (and its STM entries) by id. No-op when
    /// SQLite persistence is not enabled.
    #[uniffi::method]
    pub async fn delete_conversation(&self, id: String) -> Result<(), OneAIErrorView> {
        self.inner
            .delete_conversation(&id)
            .await
            .map_err(OneAIErrorView::from)
    }

    /// Register a tool.
    #[uniffi::method]
    pub async fn register_tool(&self, tool: Arc<OneAIToolWrapper>) -> Result<(), OneAIErrorView> {
        self.inner
            .register_tool(tool.inner.clone())
            .await
            .map_err(OneAIErrorView::from)
    }

    /// Check if a provider is configured.
    #[uniffi::method]
    pub fn has_provider(&self) -> bool {
        self.inner.has_provider()
    }

    /// Get the current platform.
    #[uniffi::method]
    pub fn platform(&self) -> PlatformView {
        PlatformView::from(*self.inner.platform())
    }

    /// Build a multi-agent group-chat session from a scenario spec.
    ///
    /// Each member carries its own provider config (kind/model/api_key), so a
    /// scenario can mix models. The session streams `speaker`-labeled
    /// `ChatEventView`s through `run_task`'s callback. See
    /// [`OneAiGroupChatSession`](crate::group_chat::OneAiGroupChatSession).
    #[uniffi::method]
    pub fn create_group_session(
        &self,
        scenario: ScenarioSpecView,
    ) -> std::result::Result<Arc<OneAiGroupChatSession>, OneAIErrorView> {
        OneAiGroupChatSession::build(scenario, &self.inner)
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

#[uniffi::export(async_runtime = "tokio")]
impl OneAISession {
    /// Get the session ID.
    #[uniffi::method]
    pub fn session_id(&self) -> String {
        self.session_id.clone()
    }

    /// Send a user message.
    ///
    /// Note: this only appends the message to the conversation — it does NOT
    /// trigger inference. To get a model reply, call `run_task` afterwards (or
    /// instead). Kept for foreign code that wants to seed context manually.
    #[uniffi::method]
    pub async fn send_user_message(&self, text: String) -> Result<(), OneAIErrorView> {
        let mut inner = self.inner.lock().await;
        inner
            .send_user_message(text)
            .await
            .map_err(OneAIErrorView::from)
    }

    /// Execute a tool by name.
    #[uniffi::method]
    pub async fn execute_tool(
        &self,
        name: String,
        args_json: String,
    ) -> Result<ToolOutputView, OneAIErrorView> {
        let inner = self.inner.lock().await;
        let args: serde_json::Value =
            serde_json::from_str(&args_json).unwrap_or(serde_json::json!({}));
        inner
            .execute_tool(&name, args)
            .await
            .map(ToolOutputView::from)
            .map_err(OneAIErrorView::from)
    }

    /// Retrieve relevant context from memory.
    #[uniffi::method]
    pub async fn retrieve_memory(
        &self,
        query: String,
        top_k: u32,
    ) -> Result<String, OneAIErrorView> {
        let inner = self.inner.lock().await;
        inner
            .retrieve_memory(&query, top_k as usize)
            .await
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| e.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
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
    #[uniffi::method]
    pub async fn run_task(
        &self,
        task: String,
        callback: Arc<dyn ChatEventCallback>,
    ) -> Result<(), OneAIErrorView> {
        tracing::info!(
            "run_task start id={} task_len={}",
            self.session_id,
            task.len()
        );
        let observer = CallbackObserver::new(callback);
        // Issue #4: `run_agent` drives the full agent loop (tools,
        // sub-agents, 3-layer parser, context assembly) — any panic in there
        // must NOT unwind through the UniFFI extern boundary (UB → process
        // abort). Catch it and surface it as a normal error result + event.
        let outcome = {
            let mut inner = self.inner.lock().await;
            std::panic::AssertUnwindSafe(inner.run_agent(
                &task,
                &observer,
                self.interrupt_slot.clone(),
            ))
            .catch_unwind()
            .await
        };
        match outcome {
            Ok(Ok(_result)) => {
                tracing::info!("run_task end ok id={}", self.session_id);
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::warn!("run_task end err id={} err={:?}", self.session_id, e);
                // Surface the error both as a return value and as an event,
                // so a foreign UI that only listens to events still sees it.
                let view = OneAIErrorView::from(e);
                observer.emit(crate::types::ChatEventView::Error {
                    message: format!("{:?}", view),
                    speaker: None,
                });
                Err(view)
            }
            Err(payload) => {
                let msg = oneai_app::panic_message(payload);
                tracing::error!(
                    "run_task caught a panic id={} panic={}",
                    self.session_id,
                    msg
                );
                // Same dual surfacing as the Err arm above.
                let view = crate::types::panic_error_view(msg);
                observer.emit(crate::types::ChatEventView::Error {
                    message: format!("{:?}", view),
                    speaker: None,
                });
                Err(view)
            }
        }
    }

    /// Reconstruct the FULL transcript as `role` + `text` views for replay
    /// into the foreign UI (after `create_session_with_id`).
    ///
    /// Merges the live (compressed) conversation with its discarded-prefix
    /// archive snapshots so the user sees the complete history — the model
    /// still sees only the bounded compressed `live` context for inference.
    /// See `MemoryManager::full_transcript_messages`. Without persistence
    /// (or on a load error), falls back to the live in-memory conversation.
    /// System/tool messages are included; the foreign UI renders only `user`
    /// and `assistant` rows.
    #[uniffi::method]
    pub async fn messages(&self) -> Vec<MessageView> {
        let inner = self.inner.lock().await;
        let live = inner.conversation();
        let full = inner
            .memory_manager()
            .full_transcript_messages(inner.session_id(), live)
            .await
            .unwrap_or_else(|_| live.messages.clone());
        full.iter().map(MessageView::from).collect()
    }

    /// The most recent `limit` display messages (the bottom of the chat) for
    /// paginated loading on session open. See `TranscriptPage`. The cursor in
    /// the returned page is passed to `transcript_older` to fetch earlier
    /// messages on demand.
    #[uniffi::method]
    pub async fn transcript_recent(&self, limit: u32) -> TranscriptPage {
        let inner = self.inner.lock().await;
        let live = inner.conversation();
        let page = inner
            .memory_manager()
            .transcript_recent(inner.session_id(), live, limit as usize)
            .await
            .unwrap_or_default();
        transcript_page_view(page)
    }

    /// Older messages immediately above `cursor` (from `transcript_recent` /
    /// a prior `transcript_older`). Returns the next page below the cursor and
    /// a new cursor for the page below that (or `None` at the top).
    #[uniffi::method]
    pub async fn transcript_older(&self, cursor: String, limit: u32) -> TranscriptPage {
        let inner = self.inner.lock().await;
        let live = inner.conversation();
        let cursor_rank: usize = cursor.parse().unwrap_or(0);
        let page = inner
            .memory_manager()
            .transcript_older(inner.session_id(), live, cursor_rank, limit as usize)
            .await
            .unwrap_or_default();
        transcript_page_view(page)
    }

    /// Persist the current in-memory conversation to SQLite immediately.
    ///
    /// `run_task` already auto-saves after the agent loop finishes, but a
    /// foreign UI may want to save mid-turn (e.g. right after the user sends
    /// their message, before the model replies, so the new chat shows up in
    /// the session list instantly). No-op (Ok) when SQLite persistence is not
    /// enabled — the auto-save path simply has nowhere to write.
    #[uniffi::method]
    pub async fn save(&self) -> Result<(), OneAIErrorView> {
        let inner = self.inner.lock().await;
        inner
            .memory_manager()
            .save_session(inner.session_id(), inner.conversation())
            .await
            .map_err(OneAIErrorView::from)
    }

    /// Request the running agent loop (if any) to interrupt at the next
    /// iteration boundary. No-op if no `run_task` is in flight.
    #[uniffi::method]
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
        session
            .send_user_message("Hello from UniFFI!".to_string())
            .await
            .unwrap();
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
        let result = session
            .execute_tool(
                "calculator".to_string(),
                "{\"expression\":\"2+3\"}".to_string(),
            )
            .await
            .unwrap();
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
            superseded: false,
            superseded_at: None,
            pinned: false,
        };
        {
            let inner = session.inner.lock().await;
            inner.memory_manager().archive_facts(vec![fact]).await;
        }

        let results = session
            .retrieve_memory("programming".to_string(), 5)
            .await
            .unwrap();
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
        let provider = Arc::new(oneai_agent::MockProvider::always_answers("Hello from mock"));
        let app_inner = oneai_app::AppBuilder::new()
            .provider(provider)
            .noop_interaction_gate()
            .default_parser()
            .build()
            .await
            .expect("build");
        let app = OneAIApp {
            inner: Arc::new(app_inner),
        };
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
                crate::types::ChatEventView::Complete { ref final_text, .. }
                    if final_text.contains("Hello")
            )),
            "expected a Complete event containing 'Hello', got: {:?}",
            events
        );
    }

    // ─── Issue #4: a panic deep in run_task must surface as an error,
    //     not abort the process at the FFI boundary ────────────────────

    /// A provider that panics on every inference — simulates the issue #4
    /// crash class (an unwrap / out-of-bounds panic deep inside a long turn).
    struct PanickingProvider {
        config: oneai_core::ModelConfig,
    }

    #[async_trait::async_trait]
    impl oneai_core::traits::LlmProvider for PanickingProvider {
        async fn infer(
            &self,
            _req: oneai_core::InferenceRequest,
        ) -> std::result::Result<oneai_core::InferenceResponse, oneai_core::OneAIError> {
            panic!("simulated deep engine panic");
        }
        async fn infer_stream(
            &self,
            _req: oneai_core::InferenceRequest,
        ) -> std::result::Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = oneai_core::InferenceStreamChunk> + Send>>,
            oneai_core::OneAIError,
        > {
            panic!("simulated deep engine panic");
        }
        fn capabilities(&self) -> oneai_core::ModelCapability {
            oneai_core::ModelCapability {
                supports_multimodal: false,
                supports_streaming: true,
                supports_tools: true,
                context_window_size: 128_000,
                max_output_tokens: 4096,
            }
        }
        fn config(&self) -> &oneai_core::ModelConfig {
            &self.config
        }
    }

    #[tokio::test]
    async fn test_run_task_catches_panic_returns_error_view() {
        let provider = Arc::new(PanickingProvider {
            config: oneai_core::ModelConfig::openai_compatible(
                "sk-test".into(),
                "https://api.test".into(),
                "panic-model".into(),
            ),
        });
        let app_inner = oneai_app::AppBuilder::new()
            .provider(provider)
            .noop_interaction_gate()
            .default_parser()
            .build()
            .await
            .expect("build");
        let app = OneAIApp {
            inner: Arc::new(app_inner),
        };
        let session = app.create_session();
        let cb = Arc::new(CollectingCallback {
            events: std::sync::Mutex::new(Vec::new()),
        });

        // The panic must surface as Err(...) — not abort the process.
        let err = session
            .run_task("trigger the panic".to_string(), cb.clone())
            .await
            .expect_err("panicking provider must yield an error view, not abort");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("internal panic") && msg.contains("simulated deep engine panic"),
            "error view must carry the panic detail: {msg}"
        );

        // An Error event must also ride the callback for event-only UIs.
        let events = cb.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| matches!(
                e,
                crate::types::ChatEventView::Error { message, .. }
                    if message.contains("panic")
            )),
            "expected an Error chat event, got: {:?}",
            events
        );

        // The session stays usable afterwards (mutex released cleanly).
        assert!(!session.session_id().is_empty());
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

    // ─── Group chat: create_group_session build/reject (no network) ────

    async fn group_app() -> Arc<OneAIApp> {
        // An app with no provider is fine — group-chat members carry their own
        // provider configs. We only exercise the build/reject seam here; a full
        // run_task needs a real provider (network).
        let app_inner = oneai_app::AppBuilder::new()
            .noop_interaction_gate()
            .default_parser()
            .build()
            .await
            .expect("build");
        Arc::new(OneAIApp {
            inner: Arc::new(app_inner),
        })
    }

    #[tokio::test]
    async fn test_create_group_session_rejects_empty_members() {
        let app = group_app().await;
        let scenario = crate::group_chat::ScenarioSpecView {
            members: Vec::new(),
            turn_policy: "roundrobin".to_string(),
            script_order: None,
            moderator_id: None,
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: None,
            locale: None,
        };
        let res = app.create_group_session(scenario);
        assert!(res.is_err(), "empty members must error");
    }

    #[tokio::test]
    async fn test_create_group_session_rejects_unknown_member_in_script_order() {
        let app = group_app().await;
        let m = crate::group_chat::AgentSpecView {
            id: "interviewer".to_string(),
            name: "面试官".to_string(),
            system_prompt: "你是面试官".to_string(),
            kind: "openai".to_string(),
            model: "gpt-4o".to_string(),
            api_key: Some("sk-test".to_string()),
            base_url: None,
            color: None,
            avatar: None,
        };
        let scenario = crate::group_chat::ScenarioSpecView {
            members: vec![m],
            turn_policy: "scripted".to_string(),
            script_order: Some(vec!["ghost".to_string()]),
            moderator_id: None,
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: None,
            locale: None,
        };
        let res = app.create_group_session(scenario);
        assert!(
            res.is_err(),
            "scripted order referencing an unknown member must error"
        );
    }

    // ─── S4: session persistence / resume / list / delete ──────────────

    /// Unique temp db path per test (no tempfile dev-dep). The file is removed
    /// first so each test starts clean.
    fn tmp_db(name: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "oneai_uniffi_{}_{}_{}.db",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        path.to_string_lossy().into_owned()
    }

    /// Build a OneAIApp with a MockProvider + SQLite persistence at `db_path`.
    async fn app_with_sqlite(db_path: &str) -> Arc<OneAIApp> {
        let provider = Arc::new(oneai_agent::MockProvider::always_answers("Hello from mock"));
        let app_inner = oneai_app::AppBuilder::new()
            .provider(provider)
            .noop_interaction_gate()
            .default_parser()
            .sqlite_persistence_at(db_path)
            .build()
            .await
            .expect("build");
        Arc::new(OneAIApp {
            inner: Arc::new(app_inner),
        })
    }

    // `CollectingCallback` is defined above (next to
    // `test_session_run_task_emits_complete`) and reused here.

    #[tokio::test]
    async fn test_sqlite_resume_round_trip() {
        let db = tmp_db("resume");
        let app = app_with_sqlite(&db).await;

        // 1. New session, run a task → auto-saves under its id.
        let session = app.create_session();
        let id = session.session_id();
        let cb = Arc::new(CollectingCallback {
            events: std::sync::Mutex::new(Vec::new()),
        });
        session
            .run_task("Say hello".to_string(), cb)
            .await
            .expect("run_task");

        // 2. list_conversations includes it.
        let list = app.list_conversations().await;
        let found = list.iter().find(|s| s.id == id);
        assert!(
            found.is_some(),
            "saved session must appear in list: {:?}",
            list
        );
        assert!(found.unwrap().message_count >= 2, "user+assistant messages");

        // 3. Resume by id → messages() replays user + assistant text.
        let resumed = app.create_session_with_id(id.to_string()).await;
        let msgs = resumed.messages().await;
        let user_text = msgs
            .iter()
            .find(|m| m.role == "user")
            .map(|m| m.text.as_str());
        let asst_text = msgs
            .iter()
            .find(|m| m.role == "assistant")
            .map(|m| m.text.as_str());
        assert_eq!(
            user_text,
            Some("Say hello"),
            "user turn must be restored: {:?}",
            msgs
        );
        assert!(
            asst_text.unwrap_or_default().contains("Hello from mock"),
            "assistant turn must be restored: {:?}",
            msgs
        );
    }

    #[tokio::test]
    async fn test_delete_conversation() {
        let db = tmp_db("delete");
        let app = app_with_sqlite(&db).await;

        let session = app.create_session();
        let id = session.session_id().to_string();
        let cb = Arc::new(CollectingCallback {
            events: std::sync::Mutex::new(Vec::new()),
        });
        session
            .run_task("hi".to_string(), cb)
            .await
            .expect("run_task");

        assert!(app.list_conversations().await.iter().any(|s| s.id == id));
        app.delete_conversation(id.clone()).await.expect("delete");
        assert!(
            !app.list_conversations().await.iter().any(|s| s.id == id),
            "deleted session must not appear in list"
        );
    }

    #[tokio::test]
    async fn test_create_session_with_id_unknown_loads_empty() {
        let db = tmp_db("unknown");
        let app = app_with_sqlite(&db).await;

        // Unknown id → empty conversation, no error.
        let resumed = app
            .create_session_with_id("never-saved-id".to_string())
            .await;
        assert_eq!(resumed.session_id(), "never-saved-id");
        assert!(
            resumed.messages().await.is_empty(),
            "unknown id must load empty history"
        );
    }
}
