//! UniFFI callback interfaces for foreign→Rust event flow.
//!
//! These traits are implemented by the foreign language (Kotlin/Swift) and
//! invoked synchronously from the Rust agent loop — callback-driven, not
//! polled. `CallbackObserver` adapts the Rust-internal `AgentLoopObserver`
//! (a `&dyn` trait that UniFFI can't cross) onto the foreign `ChatEventCallback`.

use std::sync::Arc;

use oneai_agent::{
    AgentLoopObserver, AgentLoopResult, ParadigmKind, SubAgentKind, ToolCallRequest,
};

use crate::types::ChatEventView;

// ─── ChatEventCallback (foreign-implemented) ───────────────────────

/// Foreign-implemented callback for streaming agent events.
///
/// Implement this in Kotlin/Swift and pass it to `OneAISession::run_task`.
/// `on_event` fires on the Rust tokio worker thread; UI updates must marshal
/// to the main thread (e.g. Kotlin `Handler(Looper.getMainLooper())`).
///
/// `#[uniffi::export(rust, foreign)]` (the non-deprecated successor to
/// `callback_interface` / `with_foreign`) makes the trait cross the FFI
/// boundary in both directions: foreign code implements it, and Rust impls
/// (used by the unit tests) also work.
#[uniffi::export(rust, foreign)]
pub trait ChatEventCallback: Send + Sync {
    fn on_event(&self, event: ChatEventView);
}

// ─── CallbackObserver (Rust-internal adapter) ──────────────────────

/// Adapts a foreign `ChatEventCallback` onto the `AgentLoopObserver` trait.
///
/// Each observer callback is translated to a `ChatEventView` and forwarded
/// synchronously. Events the foreign side doesn't care about (iteration start,
/// delegate, paradigm switch, checkpoint, token usage) are dropped here —
/// extend `ChatEventView` if a future UI needs them.
pub struct CallbackObserver {
    callback: Arc<dyn ChatEventCallback>,
}

impl CallbackObserver {
    pub fn new(callback: Arc<dyn ChatEventCallback>) -> Self {
        Self { callback }
    }

    pub fn emit(&self, event: ChatEventView) {
        self.callback.on_event(event);
    }
}

impl AgentLoopObserver for CallbackObserver {
    fn on_iteration_start(&self, _iteration: usize, _paradigm: ParadigmKind) {
        // No corresponding ChatEventView yet; intentionally dropped.
    }

    fn on_direct_answer(&self, text: &str) {
        self.emit(ChatEventView::DirectAnswer { text: text.to_string() });
    }

    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        for c in calls {
            self.emit(ChatEventView::ToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                args_json: c.args.to_string(),
            });
        }
    }

    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &oneai_core::ToolOutput) {
        self.emit(ChatEventView::ToolResult {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            content: output.content.clone(),
            success: output.success,
        });
    }

    fn on_delegate(&self, _task: &str, _agent_type: &SubAgentKind) {
        // Dropped — no ChatEventView variant for delegation yet.
    }

    fn on_paradigm_switch(&self, _paradigm: ParadigmKind) {
        // Dropped.
    }

    fn on_checkpoint(&self, _iteration: usize) {
        // Dropped.
    }

    fn on_complete(&self, result: &AgentLoopResult) {
        self.emit(ChatEventView::Complete {
            final_text: result.final_answer.clone(),
        });
    }

    fn on_stream_chunk(&self, text: &str) {
        self.emit(ChatEventView::StreamChunk { text: text.to_string() });
    }

    fn on_thinking(&self, text: &str) {
        self.emit(ChatEventView::Thinking { text: text.to_string() });
    }

    fn on_token_usage(&self, _prompt_tokens: u32, _completion_tokens: u32) {
        // Dropped.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Rust-side test callback that collects every event into a Vec.
    struct CollectingCallback {
        events: Mutex<Vec<ChatEventView>>,
    }

    impl CollectingCallback {
        fn new() -> Self {
            Self { events: Mutex::new(Vec::new()) }
        }

        fn take(&self) -> Vec<ChatEventView> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }
    }

    impl ChatEventCallback for CollectingCallback {
        fn on_event(&self, event: ChatEventView) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[test]
    fn test_observer_forwards_stream_chunk_and_complete() {
        let cb = Arc::new(CollectingCallback::new());
        let observer = CallbackObserver::new(cb.clone());

        observer.on_stream_chunk("Hel");
        observer.on_stream_chunk("lo");
        observer.on_complete(&AgentLoopResult {
            conversation: oneai_core::Conversation::new(),
            final_answer: "Hello".to_string(),
            global_state: oneai_core::GlobalState::default(),
            iterations: 1,
            completed: true,
            active_paradigm: ParadigmKind::ReAct,
            sub_agent_results: Vec::new(),
        });

        let events = cb.take();
        assert!(matches!(events[0], ChatEventView::StreamChunk { ref text } if text == "Hel"));
        assert!(matches!(events[1], ChatEventView::StreamChunk { ref text } if text == "lo"));
        assert!(matches!(events[2], ChatEventView::Complete { ref final_text } if final_text == "Hello"));
    }
}
