//! UniFFI-exposed group-chat session — foreign surface for the engine
//! `oneai_agent::GroupChatSession` primitive.
//!
//! A scenario (cast of persona agents + turn policy) is built on the foreign
//! side as a `ScenarioSpecView`, handed to `OneAIApp::create_group_session`,
//! which constructs a per-member provider + a shared tools/parser/gate from
//! the app and builds the engine `GroupChatSession`. `run_task` /
//! `start` stream `speaker`-labeled `ChatEventView`s back through the foreign
//! `ChatEventCallback`.

use std::sync::Arc;

use oneai_agent::group_chat::{
    GroupChatConfig, GroupChatMemberSpec, GroupChatObserver, GroupChatPersistence,
    GroupChatResources, GroupChatSession, TurnPolicy,
};
use oneai_agent::{AgentLoopObserver, AgentLoopResult, ParadigmKind, SubAgentKind, ToolCallRequest};
use oneai_core::error::Result;
use oneai_core::Conversation;

use crate::callback::ChatEventCallback;
use crate::types::{ChatEventView, MessageView, OneAIErrorView};

// ─── Spec records ────────────────────────────────────────────────────────────

/// One persona in a scenario.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AgentSpecView {
    /// Stable member id (referenced by turn policies / opener).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Persona system prompt.
    pub system_prompt: String,
    /// Provider kind: `"openai"` / `"anthropic"` / `"ollama"`.
    pub kind: String,
    /// Model name (e.g. `gpt-4o`, `claude-sonnet-4-6`).
    pub model: String,
    /// API key (required for openai/anthropic; ignored for ollama).
    pub api_key: Option<String>,
    /// Base URL override (OpenAI-compatible endpoints).
    pub base_url: Option<String>,
    /// UI accent color hint (e.g. `"#4D6BFE"`). Opaque to the engine.
    pub color: Option<String>,
    /// UI avatar SF-symbol hint. Opaque to the engine.
    pub avatar: Option<String>,
}

/// A multi-agent scenario spec handed to `OneAIApp::create_group_session`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ScenarioSpecView {
    /// The cast (each member carries its own provider config).
    pub members: Vec<AgentSpecView>,
    /// `"scripted"` | `"roundrobin"` | `"moderator"`.
    pub turn_policy: String,
    /// `scripted` only — member ids to run in order after each user input.
    pub script_order: Option<Vec<String>>,
    /// `moderator` only — member id that picks the next speaker.
    pub moderator_id: Option<String>,
    /// Member id that delivers the opening turn (`None` = user speaks first).
    pub opener_agent_id: Option<String>,
    /// Seed line for the opener.
    pub opener_line: Option<String>,
    /// Optional conversation title (e.g. `"面试演练·前端工程师"`). Persisted into
    /// `Conversation.metadata["title"]` so the saved session is named after the
    /// scenario instead of falling back to "新对话".
    pub title: Option<String>,
}

// ─── OneAiGroupChatSession ────────────────────────────────────────────────────

/// Foreign wrapper over the engine `GroupChatSession`.
#[derive(uniffi::Object)]
pub struct OneAiGroupChatSession {
    inner: Arc<GroupChatSession>,
    /// Persistence hook bound to the app's memory manager (SQLite-backed when
    /// `sqlite_persistence_at` was set on the builder).
    persistence: Option<Arc<GroupChatPersistenceImpl>>,
}

/// Build an `Arc<dyn LlmProvider>` from a foreign `AgentSpecView` — mirrors
/// `OneAIAppBuilder::provider_config` so each member can use a different model.
fn build_member_provider(spec: &AgentSpecView) -> std::result::Result<Arc<dyn oneai_core::traits::LlmProvider>, OneAIErrorView> {
    let provider: Arc<dyn oneai_core::traits::LlmProvider> = match spec.kind.as_str() {
        "openai" => {
            let config = oneai_core::ModelConfig::openai_compatible(
                spec.api_key.clone().unwrap_or_default(),
                spec.base_url.clone().unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
                spec.model.clone(),
            );
            Arc::new(oneai_provider::OpenAIProvider::new(config))
        }
        "anthropic" => {
            let config = oneai_core::ModelConfig::anthropic(
                spec.api_key.clone().unwrap_or_default(),
                spec.model.clone(),
            );
            Arc::new(oneai_provider::AnthropicProvider::new(config))
        }
        "ollama" => {
            let config = if let Some(base) = &spec.base_url {
                // base_url carries "host:port" for ollama (matching the macOS
                // settings convention) — split into host/port.
                let (host, port) = match base.rsplit_once(':') {
                    Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(11434)),
                    None => (base.clone(), 11434),
                };
                oneai_core::ModelConfig::ollama_custom(host, port, spec.model.clone())
            } else {
                oneai_core::ModelConfig::ollama(spec.model.clone())
            };
            Arc::new(oneai_provider::OllamaProvider::new(config))
        }
        other => {
            return Err(OneAIErrorView::Config {
                message: format!("Unknown provider kind '{}' for member '{}'; expected openai/anthropic/ollama", other, spec.id),
            });
        }
    };
    Ok(provider)
}

impl OneAiGroupChatSession {
    /// Build the FFI wrapper from a scenario spec + the app's shared
    /// (non-provider) resources. Each member gets its own provider from
    /// `AgentSpecView`.
    pub(crate) fn build(
        scenario: ScenarioSpecView,
        app: &oneai_app::App,
    ) -> std::result::Result<Arc<Self>, OneAIErrorView> {
        if scenario.members.is_empty() {
            return Err(OneAIErrorView::Config {
                message: "scenario needs ≥1 member".to_string(),
            });
        }

        // Per-member providers.
        let mut providers = std::collections::HashMap::new();
        for m in &scenario.members {
            providers.insert(m.id.clone(), build_member_provider(m)?);
        }

        // Shared resources from the app.
        let resources = GroupChatResources {
            providers,
            tools: app.tool_executor.tools_map(),
            parser: app.parser.clone(),
            interaction_gate: app.interaction_gate.clone(),
        };

        // Engine config.
        let members: Vec<GroupChatMemberSpec> = scenario
            .members
            .iter()
            .map(|m| GroupChatMemberSpec {
                id: m.id.clone(),
                name: m.name.clone(),
                system_prompt: m.system_prompt.clone(),
            })
            .collect();
        let turn_policy = match scenario.turn_policy.as_str() {
            "scripted" => TurnPolicy::Scripted {
                order: scenario.script_order.unwrap_or_default(),
            },
            "moderator" => TurnPolicy::Moderator {
                moderator_id: scenario.moderator_id.clone().unwrap_or_default(),
                max_turns: 6,
            },
            _ => TurnPolicy::RoundRobin,
        };
        let config = GroupChatConfig {
            members,
            turn_policy,
            opener_agent_id: scenario.opener_agent_id,
            opener_line: scenario.opener_line,
            title: scenario.title,
        };

        let session = GroupChatSession::new(config, resources)
            .map_err(|e| OneAIErrorView::Config { message: format!("{:?}", e) })?;

        // Persistence via the app's memory manager (SQLite-backed when
        // sqlite_persistence_at was set; otherwise save is a no-op).
        let persistence = Arc::new(GroupChatPersistenceImpl {
            memory: app.memory_manager.clone(),
        });
        let session = session.with_persistence(persistence.clone());
        let inner = Arc::new(session);

        Ok(Arc::new(Self {
            inner,
            persistence: Some(persistence),
        }))
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl OneAiGroupChatSession {
    /// Run the opener turn (if the scenario configured one). No user message
    /// is added. Call before the first `run_task`. Emits speaker-labeled
    /// events through `callback`.
    pub async fn start(
        &self,
        callback: Arc<dyn ChatEventCallback>,
    ) -> std::result::Result<(), OneAIErrorView> {
        let observer = GroupChatCallbackObserver::new(callback);
        self.inner
            .start(&observer)
            .await
            .map_err(OneAIErrorView::from)
    }

    /// Append the user's message and run the round's speakers per the turn
    /// policy until it's the user's turn again. Streams `speaker`-labeled
    /// events through `callback` (fires on the tokio worker thread — the
    /// foreign callback must marshal UI updates to the main thread).
    pub async fn run_task(
        &self,
        user_input: String,
        callback: Arc<dyn ChatEventCallback>,
    ) -> std::result::Result<(), OneAIErrorView> {
        let observer = GroupChatCallbackObserver::new(callback);
        self.inner
            .run_task(&user_input, &observer)
            .await
            .map_err(OneAIErrorView::from)
    }

    /// Request the running member to interrupt at the next iteration boundary.
    pub async fn interrupt(&self) {
        self.inner.interrupt();
    }

    /// Switch the turn policy to a fixed scripted order at runtime. Used by
    /// scenarios that change speakers mid-conversation — e.g. an interview
    /// scenario calls this with `["coach"]` to drop the interviewer and go
    /// coach-only for the debrief phase (coach summarizes, then takes the
    /// user's follow-up questions). The next `run_task` honors the new order.
    pub async fn set_scripted_order(&self, order: Vec<String>) {
        self.inner
            .set_turn_policy(TurnPolicy::Scripted { order })
            .await;
    }

    /// Snapshot the shared conversation as speaker-labeled message views (for
    /// replaying a resumed scenario session).
    pub async fn messages(&self) -> Vec<MessageView> {
        let conv = self.inner.conversation().await;
        conv.messages.iter().map(MessageView::from).collect()
    }

    /// Persist the shared conversation immediately (no-op when SQLite
    /// persistence is not enabled). `run_task` already auto-saves after each
    /// round; this is for mid-round saves.
    pub async fn save(&self) -> std::result::Result<(), OneAIErrorView> {
        if let Some(p) = &self.persistence {
            let conv = self.inner.conversation().await;
            p.save_conversation(&conv).await.map_err(OneAIErrorView::from)?;
        }
        Ok(())
    }
}

// ─── GroupChatPersistenceImpl ────────────────────────────────────────────────

struct GroupChatPersistenceImpl {
    memory: Arc<oneai_memory::MemoryManager>,
}

impl GroupChatPersistence for GroupChatPersistenceImpl {
    fn save_conversation<'a>(
        &'a self,
        conversation: &'a Conversation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.memory
                .save_session(&conversation.id, conversation)
                .await
        })
    }
}

// ─── GroupChatCallbackObserver ───────────────────────────────────────────────

/// Adapts a foreign `ChatEventCallback` onto the engine `GroupChatObserver`
/// trait. Tracks the current speaker (set by `on_speaker_turn` before each
/// member's run) and emits `speaker`-labeled `ChatEventView`s for every
/// forwarded `AgentLoopObserver` callback — so the UI can route fragments to
/// the correct member's bubble.
pub struct GroupChatCallbackObserver {
    callback: Arc<dyn ChatEventCallback>,
    /// `std::sync::Mutex` (not tokio): the observer callbacks are *synchronous*
    /// trait methods firing on the tokio worker thread mid-`run_with_conversation`,
    /// and the lock is never held across an `.await` — only a trivial
    /// assign/clone. `tokio::sync::Mutex::blocking_lock()` from a sync callback
    /// risks blocking a runtime worker; a std Mutex is the correct tool here.
    current_speaker: std::sync::Mutex<String>,
}

impl GroupChatCallbackObserver {
    pub fn new(callback: Arc<dyn ChatEventCallback>) -> Self {
        Self {
            callback,
            current_speaker: std::sync::Mutex::new(String::new()),
        }
    }
}

impl AgentLoopObserver for GroupChatCallbackObserver {
    fn on_iteration_start(&self, _: usize, _: ParadigmKind) {}

    fn on_direct_answer(&self, text: &str) {
        let sp = self.speaker_sync();
        self.callback.on_event(ChatEventView::DirectAnswer {
            text: text.to_string(),
            speaker: sp,
        });
    }

    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        let sp = self.speaker_sync();
        for c in calls {
            self.callback.on_event(ChatEventView::ToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                args_json: c.args.to_string(),
                speaker: sp.clone(),
            });
        }
    }

    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &oneai_core::ToolOutput) {
        let sp = self.speaker_sync();
        self.callback.on_event(ChatEventView::ToolResult {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            content: output.content.clone(),
            success: output.success,
            speaker: sp,
        });
    }

    fn on_delegate(&self, _: &str, _: &SubAgentKind) {}
    fn on_paradigm_switch(&self, _: ParadigmKind) {}
    fn on_checkpoint(&self, _: usize) {}

    fn on_complete(&self, result: &AgentLoopResult) {
        let sp = self.speaker_sync();
        self.callback.on_event(ChatEventView::Complete {
            final_text: result.final_answer.clone(),
            speaker: sp,
        });
    }

    fn on_stream_chunk(&self, text: &str) {
        let sp = self.speaker_sync();
        self.callback.on_event(ChatEventView::StreamChunk {
            text: text.to_string(),
            speaker: sp,
        });
    }

    fn on_thinking(&self, text: &str) {
        let sp = self.speaker_sync();
        self.callback.on_event(ChatEventView::Thinking {
            text: text.to_string(),
            speaker: sp,
        });
    }
}

impl GroupChatObserver for GroupChatCallbackObserver {
    fn on_speaker_turn(&self, speaker: &str) {
        // Trivial critical section — std Mutex, no await held. Fires on the
        // tokio worker thread between member runs (not a hot path).
        *self.current_speaker.lock().unwrap() = speaker.to_string();
    }
}

impl GroupChatCallbackObserver {
    /// Read the current speaker id (the observer callbacks fire on the worker
    /// thread, never contended).
    fn speaker_sync(&self) -> Option<String> {
        let g = self.current_speaker.lock().unwrap();
        if g.is_empty() { None } else { Some(g.clone()) }
    }
}
