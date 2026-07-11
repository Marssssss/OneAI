//! GroupChatSession — shared-transcript multi-agent conversation primitive.
//!
//! Unlike [`TeamCoordinator`](crate::team), which *aggregates* (fan out to N
//! agents → merge into one result), a group chat is a *dialogue*: N persona
//! agents take turns speaking inside ONE shared conversation, with a human in
//! the loop. This is the AutoGen "GroupChat" / Coze multi-agent-conversation
//! pattern, lifted into the engine so every native port gets it for free
//! instead of reimplementing orchestration in UI code.
//!
//! Design:
//! - Each member is a lean [`AgentLoop`] (persona `system_prompt`, shared
//!   provider/tools/parser) built the same way [`DefaultSubAgentFactory`]
//!   builds sub-agents — no domain-pack / paradigm / fact-extraction machinery
//!   (members are conversational, not tool-heavy executors).
//! - One shared [`Conversation`] holds the dialogue. Each member's turn runs
//!   over a *derived* transcript (shared minus system messages) so the member's
//!   own persona system prompt is injected fresh by the loop; only the member's
//!   final answer is appended back to the shared transcript as an assistant
//!   message tagged `metadata["speaker"] = <member id>`.
//! - A [`GroupChatObserver`] extends [`AgentLoopObserver`] with
//!   `on_speaker_turn(speaker)` — called before each member's run so the
//!   observer impl knows which member produced the events it is about to
//!   receive (the FFI layer emits `speaker`-labeled `ChatEventView`s from this).
//! - Turn policies: [`TurnPolicy::Scripted`] (fixed order after each user
//!   input), [`TurnPolicy::RoundRobin`] (member-list order), and
//!   [`TurnPolicy::Moderator`] (a moderator member picks the next speaker).
//!
//! The primitive is engine-only here; the FFI surface
//! (`OneAiGroupChatSession`) lives in `oneai-uniffi`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use oneai_core::budget::{
    BudgetAllocation, ContextBudgetManager, TokenBudget, TruncationCompressor,
};
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{InteractionGate, LlmProvider, OutputParser, Tool};
use oneai_core::{Conversation, Message, Role};
use oneai_persistence::ProgressiveCheckpointManager;
use oneai_skill::SkillSelector;

use crate::agent_loop::{
    AgentLoop, AgentLoopConfig, AgentLoopObserver, AgentLoopResult,
};
use crate::context_assembler::ContextAssembler;
use crate::streaming::IncrementalStreamParser;
use crate::sub_agent::SubAgentFactoryNone;

// ─── Persistence hook ───────────────────────────────────────────────────────

/// Persistence seam for a group-chat conversation.
///
/// The engine stays free of SQLite; the FFI layer implements this against the
/// app's memory manager / SQLite store (mirroring `OneAiSession::save`).
/// `save` is called after each completed turn so the shared transcript —
/// including `metadata["speaker"]` tags — survives a restart and replays with
/// speaker identity intact.
pub trait GroupChatPersistence: Send + Sync {
    /// Persist the current shared conversation.
    fn save_conversation<'a>(
        &'a self,
        conversation: &'a Conversation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}

// ─── GroupChatObserver ───────────────────────────────────────────────────────

/// Observer for group-chat execution.
///
/// Extends [`AgentLoopObserver`] (forwarded for the *currently speaking*
/// member) with a speaker-boundary callback. The FFI layer's
/// `GroupChatCallbackObserver` records the current speaker from
/// `on_speaker_turn` and emits `speaker`-labeled `ChatEventView`s for every
/// forwarded observer callback.
pub trait GroupChatObserver: AgentLoopObserver {
    /// Called before a member's agent loop starts running. `speaker` is the
    /// member id about to produce events. Default no-op so a plain
    /// `AgentLoopObserver` impl still typechecks as a stand-in for tests.
    fn on_speaker_turn(&self, _speaker: &str) {}
}

// ─── Config ──────────────────────────────────────────────────────────────────

/// A member persona specification (pre-build).
#[derive(Debug, Clone)]
pub struct GroupChatMemberSpec {
    /// Stable member id (referenced by turn policies / opener).
    pub id: String,
    /// Display name (also used as the speaker label in `metadata["speaker"]`).
    pub name: String,
    /// Persona system prompt for this member's `AgentLoopConfig`.
    pub system_prompt: String,
}

/// How the next speaker is chosen after the user speaks.
#[derive(Debug, Clone, Default)]
pub enum TurnPolicy {
    /// Run these member ids in order after each user input, then stop and wait
    /// for the next user message. The interview case: `[coach, interviewer]`
    /// → coach critiques, interviewer asks the next question, then it's the
    /// user's turn again.
    Scripted { order: Vec<String> },
    /// Each member speaks once in [`GroupChatConfig::members`] order after each
    /// user input, then stop.
    #[default]
    RoundRobin,
    /// A moderator member decides the next speaker after each turn. The
    /// moderator runs its own agent loop over the transcript and returns a
    /// member id (or `"user"` to hand back to the human). Stops at `"user"` or
    /// after `max_turns` moderator picks (safety bound).
    Moderator { moderator_id: String, max_turns: usize },
}

/// Shared engine resources a group chat runs on (mirrors what
/// `DefaultSubAgentFactory` needs to build a member loop). The provider is
/// per-member (keyed by member id) so a scenario can mix models — e.g. a
/// Claude interviewer + a GPT-4o coach. Tools/parser/gate are shared.
pub struct GroupChatResources {
    pub providers: HashMap<String, Arc<dyn LlmProvider>>,
    pub tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    pub parser: Arc<dyn OutputParser>,
    pub interaction_gate: Arc<dyn InteractionGate>,
}

/// Group-chat configuration.
#[derive(Clone)]
pub struct GroupChatConfig {
    pub members: Vec<GroupChatMemberSpec>,
    pub turn_policy: TurnPolicy,
    /// Member id that delivers the opening turn (before the first user
    /// message). `None` = the user speaks first.
    pub opener_agent_id: Option<String>,
    /// Seed line handed to the opener as its task (e.g. "开始一场算法面试").
    pub opener_line: Option<String>,
    /// Optional human-readable title for the shared conversation. When set,
    /// written into `Conversation.metadata["title"]` so the persistence layer
    /// titles the saved session (e.g. "面试演练·前端工程师") instead of
    /// falling back to "新对话" — group chats rarely carry a first user
    /// message for the default first-user-message title derivation.
    pub title: Option<String>,
}

// ─── GroupChatSession ────────────────────────────────────────────────────────

/// Shared-transcript multi-agent conversation primitive.
///
/// Holds N pre-built persona `AgentLoop`s over one shared `Conversation`.
/// Build via [`GroupChatSession::new`]; drive via [`start`](Self::start) /
/// [`run_task`](Self::run_task); interrupt via [`interrupt`](Self::interrupt).
pub struct GroupChatSession {
    config: GroupChatConfig,
    conversation: Mutex<Conversation>,
    loops: HashMap<String, AgentLoop>,
    /// Clone of the currently-running member loop — `interrupt` flips its
    /// shared interrupt flag (Arc-backed, so the clone shares state with the
    /// loop the worker is actually running).
    running_loop: Mutex<Option<AgentLoop>>,
    /// Tracks scripted/round-robin position across calls.
    turn_cursor: AtomicUsize,
    interrupt_flag: Arc<AtomicBool>,
    persistence: Option<Arc<dyn GroupChatPersistence>>,
    /// Live turn policy — initialized from `config.turn_policy` but mutable at
    /// runtime via [`set_turn_policy`](Self::set_turn_policy). This lets a
    /// scenario change speakers mid-conversation (e.g. an interview scenario
    /// drops the interviewer and goes coach-only for the debrief phase).
    /// Guarded by an async mutex because `speakers_for_round` is async.
    turn_policy: tokio::sync::Mutex<TurnPolicy>,
}

impl GroupChatSession {
    /// Build the session: constructs a lean `AgentLoop` per member persona,
    /// all sharing the provided resources (provider/tools/parser/gate).
    pub fn new(config: GroupChatConfig, resources: GroupChatResources) -> Result<Self> {
        if config.members.is_empty() {
            return Err(OneAIError::Config("group chat needs ≥1 member".into()));
        }
        // Validate referenced ids exist.
        let ids: std::collections::HashSet<&str> =
            config.members.iter().map(|m| m.id.as_str()).collect();
        match &config.turn_policy {
            TurnPolicy::Scripted { order } => {
                for id in order {
                    if !ids.contains(id.as_str()) {
                        return Err(OneAIError::Config(format!(
                            "scripted order references unknown member '{id}'"
                        )));
                    }
                }
            }
            TurnPolicy::Moderator { moderator_id, .. } => {
                if !ids.contains(moderator_id.as_str()) {
                    return Err(OneAIError::Config(format!(
                        "moderator '{moderator_id}' is not a member"
                    )));
                }
            }
            _ => {}
        }
        if let Some(op) = &config.opener_agent_id {
            if !ids.contains(op.as_str()) {
                return Err(OneAIError::Config(format!(
                    "opener '{op}' is not a member"
                )));
            }
        }

        let mut loops = HashMap::new();
        for m in &config.members {
            let provider = resources.providers.get(&m.id).cloned().ok_or_else(|| {
                OneAIError::Config(format!("no provider for member '{}'", m.id))
            })?;
            loops.insert(m.id.clone(), build_member_loop(m, &resources, provider));
        }

        // Seed the shared conversation's title metadata (if configured) so the
        // persistence layer names the saved session after the scenario.
        let mut conversation = Conversation::new();
        if let Some(title) = &config.title {
            conversation
                .metadata
                .insert("title".to_string(), title.clone());
        }

        // Live turn policy, initialized from the config's initial policy.
        let turn_policy = tokio::sync::Mutex::new(config.turn_policy.clone());

        Ok(Self {
            config,
            conversation: Mutex::new(conversation),
            loops,
            running_loop: Mutex::new(None),
            turn_cursor: AtomicUsize::new(0),
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            persistence: None,
            turn_policy,
        })
    }

    /// Attach a persistence hook (called after each completed turn).
    pub fn with_persistence(mut self, p: Arc<dyn GroupChatPersistence>) -> Self {
        self.persistence = Some(p);
        self
    }

    /// Borrow the shared conversation (FFI reads this for `messages()` /
    /// `save()`).
    pub async fn conversation(&self) -> tokio::sync::MutexGuard<'_, Conversation> {
        self.conversation.lock().await
    }

    /// Run the opener turn (if configured). No user message is added; the
    /// opener's opening line becomes the first assistant turn. No-op if no
    /// opener is configured.
    pub async fn start(&self, observer: &dyn GroupChatObserver) -> Result<()> {
        let Some(opener_id) = self.config.opener_agent_id.clone() else {
            return Ok(());
        };
        let task = self
            .config
            .opener_line
            .clone()
            .unwrap_or_else(|| "请以你的角色身份开始这场对话。".to_string());
        self.run_member(&opener_id, task, observer).await?;
        self.persist().await;
        Ok(())
    }

    /// Append the user's message, then run speakers per the turn policy until
    /// it's the user's turn again. Emits speaker-labeled events through
    /// `observer`.
    pub async fn run_task(
        &self,
        user_input: &str,
        observer: &dyn GroupChatObserver,
    ) -> Result<()> {
        if self.interrupt_flag.load(Ordering::Relaxed) {
            self.interrupt_flag.store(false, Ordering::Relaxed);
        }

        // 1. Append the user's message to the shared transcript.
        {
            let mut conv = self.conversation.lock().await;
            let mut msg = Message::user(user_input.to_string());
            msg.metadata.insert("speaker".to_string(), "user".to_string());
            conv.add_message(msg);
        }

        // 2. Determine the speaker sequence for this round.
        let speakers = self.speakers_for_round().await?;
        let mut first = true;
        for member_id in speakers {
            if self.interrupt_flag.load(Ordering::Relaxed) {
                break;
            }
            // First speaker responds directly to the user input (already in the
            // shared transcript as the last user message); subsequent speakers
            // get a role nudge so they continue rather than re-answering.
            let task = if first {
                format!("（用户刚发来消息，请以 {} 的身份作出回应。）", member_id)
            } else {
                format!("（现在轮到你（{}）发言，请结合上文继续对话。）", member_id)
            };
            first = false;
            match self.run_member(&member_id, task, observer).await {
                Ok(()) => {}
                Err(OneAIError::Other(msg)) if msg.contains("interrupted") => break,
                Err(e) => return Err(e),
            }
        }

        self.persist().await;
        Ok(())
    }

    /// Request the running member loop to interrupt at the next iteration
    /// boundary. The current turn completes its in-flight speaker; subsequent
    /// speakers in the round are skipped.
    pub fn interrupt(&self) {
        self.interrupt_flag.store(true, Ordering::Relaxed);
        // If a member loop is running, flip its shared interrupt flag (the
        // Arc-backed fields are shared with the clone the worker is running).
        // try_lock avoids blocking when the worker holds the slot.
        if let Ok(slot) = self.running_loop.try_lock() {
            if let Some(loop_) = slot.as_ref() {
                loop_.request_interrupt(oneai_core::InterruptReason::Custom {
                    reason: "group-chat user interrupt".into(),
                });
            }
        }
    }

    // ─── internals ───────────────────────────────────────────────────────

    /// Run one member's turn over a derived transcript.
    async fn run_member(
        &self,
        member_id: &str,
        task: String,
        observer: &dyn GroupChatObserver,
    ) -> Result<()> {
        let member_loop = self.loops.get(member_id).ok_or_else(|| {
            OneAIError::Config(format!("unknown member '{member_id}'"))
        })?;

        // Tell the observer who is about to speak.
        observer.on_speaker_turn(member_id);

        // Derived transcript: shared minus system messages, so the loop
        // injects this member's persona system prompt fresh.
        let derived = {
            let conv = self.conversation.lock().await;
            let mut d = Conversation::with_id(conv.id.clone());
            for m in &conv.messages {
                if m.role != Role::System {
                    d.add_message(m.clone());
                }
            }
            d
        };

        // Register the running loop so interrupt() can reach it.
        {
            let mut slot = self.running_loop.lock().await;
            *slot = Some(member_loop.clone());
        }

        let result = member_loop
            .run_with_conversation(derived, &task, observer)
            .await;

        {
            let mut slot = self.running_loop.lock().await;
            *slot = None;
        }

        let result: AgentLoopResult = result?;

        // Append this member's answer to the shared transcript, tagged with
        // the speaker id.
        if !result.final_answer.is_empty() {
            let mut conv = self.conversation.lock().await;
            let mut msg = Message::assistant(result.final_answer);
            msg.metadata
                .insert("speaker".to_string(), member_id.to_string());
            conv.add_message(msg);
        }
        Ok(())
    }

    /// Replace the turn policy at runtime. Used by scenarios that change
    /// speakers mid-conversation — e.g. an interview scenario switching to a
    /// coach-only scripted order for the debrief phase (interviewer drops out,
    /// coach summarizes and takes follow-up questions). The next `run_task`
    /// uses the new policy.
    pub async fn set_turn_policy(&self, policy: TurnPolicy) {
        *self.turn_policy.lock().await = policy;
    }

    /// Resolve the speaker sequence for the current round per the turn policy.
    async fn speakers_for_round(&self) -> Result<Vec<String>> {
        // Clone the live policy so the async mutex is not held across the
        // moderator loop's awaits below.
        let turn_policy = self.turn_policy.lock().await.clone();
        match turn_policy {
            TurnPolicy::Scripted { order } => Ok(order.clone()),
            TurnPolicy::RoundRobin => {
                // Each non-opener member speaks once per round, in member-list
                // order, starting at the rotating cursor.
                let n = self.config.members.len();
                if n == 0 {
                    return Ok(Vec::new());
                }
                let start = self.turn_cursor.fetch_add(1, Ordering::Relaxed) % n;
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(self.config.members[(start + i) % n].id.clone());
                }
                Ok(out)
            }
            TurnPolicy::Moderator { moderator_id, max_turns } => {
                // Run the moderator repeatedly; it returns the next speaker id
                // (or "user"). Each pick is one member turn. Bounded by
                // max_turns to guarantee termination.
                let mut out = Vec::new();
                let guard = self.conversation.lock().await;
                let transcript = guard.clone();
                drop(guard);
                for _ in 0..max_turns {
                    let pick = self.moderator_pick(&moderator_id, &transcript).await?;
                    if pick == "user" || pick.is_empty() {
                        break;
                    }
                    if !self.config.members.iter().any(|m| m.id == pick) {
                        // Unknown pick — stop to avoid a loop.
                        break;
                    }
                    out.push(pick);
                }
                Ok(out)
            }
        }
    }

    /// Ask the moderator member to choose the next speaker.
    async fn moderator_pick(&self, moderator_id: &str, transcript: &Conversation) -> Result<String> {
        let member_loop = self.loops.get(moderator_id).ok_or_else(|| {
            OneAIError::Config(format!("moderator '{moderator_id}' not found"))
        })?;
        // Build a derived transcript (no system msgs) + a moderator system
        // prompt is already the member's persona. Append a pick instruction as
        // the task.
        let mut derived = Conversation::with_id(transcript.id.clone());
        for m in &transcript.messages {
            if m.role != Role::System {
                derived.add_message(m.clone());
            }
        }
        let member_ids: Vec<String> = self.config.members.iter().map(|m| m.id.clone()).collect();
        let task = format!(
            "你是这场对话的主持人。根据当前对话进展，选择下一位应该发言的角色。\n可选角色: {} 或 \"user\"（把发言权交还给用户）。\n只回复该角色的 id，不要回复其他内容。",
            member_ids.join(", ")
        );
        // Run the moderator silently (no UI events) using a SilentObserver,
        // then parse its final answer as the picked member id.
        let silent = SilentGroupObserver;
        let result = member_loop
            .run_with_conversation(derived, &task, &silent)
            .await?;
        Ok(result.final_answer.trim().trim_matches('"').to_string())
    }

    async fn persist(&self) {
        if let Some(p) = &self.persistence {
            let conv = self.conversation.lock().await;
            let _ = p.save_conversation(&conv).await;
        }
    }
}

// ─── SilentGroupObserver (for moderator runs) ───────────────────────────────

struct SilentGroupObserver;

impl AgentLoopObserver for SilentGroupObserver {
    fn on_iteration_start(&self, _: usize, _: crate::agent_loop::ParadigmKind) {}
    fn on_direct_answer(&self, _: &str) {}
    fn on_tool_calls(&self, _: &[crate::agent_loop::ToolCallRequest]) {}
    fn on_tool_result(&self, _: &str, _: &str, _: &oneai_core::ToolOutput) {}
    fn on_delegate(&self, _: &str, _: &crate::sub_agent::SubAgentKind) {}
    fn on_paradigm_switch(&self, _: crate::agent_loop::ParadigmKind) {}
    fn on_checkpoint(&self, _: usize) {}
    fn on_complete(&self, _: &AgentLoopResult) {}
    fn on_stream_chunk(&self, _: &str) {}
    fn on_thinking(&self, _: &str) {}
}

impl GroupChatObserver for SilentGroupObserver {}

// ─── build_member_loop ───────────────────────────────────────────────────────

/// Build a lean `AgentLoop` for one persona member — same shape as
/// `DefaultSubAgentFactory::create` (no domain pack, no fact extraction, no
/// nested delegation), differing only in `system_prompt` and `use_streaming`.
fn build_member_loop(
    m: &GroupChatMemberSpec,
    resources: &GroupChatResources,
    provider: Arc<dyn LlmProvider>,
) -> AgentLoop {
    let config = AgentLoopConfig {
        system_prompt: m.system_prompt.clone(),
        use_streaming: true,
        temperature: Some(0.7),
        top_p: None,
        max_tokens: None,
        thinking_budget: None,
        stop_sequences: Vec::new(),
        // Persona members are conversational, not tool-heavy executors — a tight
        // bound prevents a misbehaving turn from looping endlessly and flooding
        // the UI with stream events (which can beachball the main thread).
        hard_max_iterations: Some(15),
        auto_checkpoint: false,
        inject_skills: false,
        usage_tracker: None,
        rate_limiter: None,
        circuit_breaker: None,
        token_counter: None,
        structured_output: None,
        constrained_output_policy: oneai_core::ConstrainedOutputPolicy::Auto,
        trace_context: None,
        plan_mode: false,
    };
    let context_assembler = ContextAssembler::new();
    let stream_parser = IncrementalStreamParser::new();
    let budget = TokenBudget::new(100_000);
    AgentLoop::new(
        provider,
        resources.tools.clone(),
        resources.parser.clone(),
        resources.interaction_gate.clone(),
        Arc::new(SkillSelector::new()),
        Arc::new(ContextBudgetManager::new(
            budget,
            BudgetAllocation::default(),
            Arc::new(TruncationCompressor::default()),
        )),
        Arc::new(SubAgentFactoryNone),
        context_assembler,
        stream_parser,
        None::<Arc<ProgressiveCheckpointManager>>,
        config,
    )
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_provider::{MockProvider, ScriptedResponse};
    use std::sync::Mutex;

    /// A collecting observer that records (speaker, stream-chunk) pairs so we
    /// can assert which member produced which text.
    struct RecordingObserver {
        chunks: Mutex<Vec<(String, String)>>,
        current: Mutex<String>,
    }
    impl RecordingObserver {
        fn new() -> Self { Self { chunks: Mutex::new(vec![]), current: Mutex::new(String::new()) } }
    }
    impl AgentLoopObserver for RecordingObserver {
        fn on_iteration_start(&self, _: usize, _: crate::agent_loop::ParadigmKind) {}
        fn on_stream_chunk(&self, t: &str) {
            let cur = self.current.lock().unwrap().clone();
            self.chunks.lock().unwrap().push((cur.clone(), t.to_string()));
        }
        fn on_direct_answer(&self, _: &str) {}
        fn on_tool_calls(&self, _: &[crate::agent_loop::ToolCallRequest]) {}
        fn on_tool_result(&self, _: &str, _: &str, _: &oneai_core::ToolOutput) {}
        fn on_delegate(&self, _: &str, _: &crate::sub_agent::SubAgentKind) {}
        fn on_paradigm_switch(&self, _: crate::agent_loop::ParadigmKind) {}
        fn on_checkpoint(&self, _: usize) {}
        fn on_complete(&self, _: &AgentLoopResult) {}
        fn on_thinking(&self, _: &str) {}
    }
    impl GroupChatObserver for RecordingObserver {
        fn on_speaker_turn(&self, speaker: &str) {
            *self.current.lock().unwrap() = speaker.to_string();
        }
    }

    fn resources(provider: Arc<dyn LlmProvider>) -> GroupChatResources {
        // Tests: every member shares the same mock provider.
        let mut providers = HashMap::new();
        providers.insert("interviewer".to_string(), provider.clone());
        providers.insert("coach".to_string(), provider.clone());
        providers.insert("a".to_string(), provider);
        GroupChatResources {
            providers,
            tools: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            parser: Arc::new(oneai_parser::ThreeLayerParser::new()),
            interaction_gate: Arc::new(oneai_tool::NoopInteractionGate),
        }
    }

    #[tokio::test]
    async fn scripted_two_member_round_tags_speakers() {
        // Two scripted members; after user input both speak in order.
        let provider = Arc::new(MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("面试官的问题"),
            ScriptedResponse::direct_answer("指导员的点评"),
        ]));
        let cfg = GroupChatConfig {
            members: vec![
                GroupChatMemberSpec { id: "interviewer".into(), name: "面试官".into(), system_prompt: "你是面试官".into() },
                GroupChatMemberSpec { id: "coach".into(), name: "指导员".into(), system_prompt: "你是指导员".into() },
            ],
            turn_policy: TurnPolicy::Scripted { order: vec!["interviewer".into(), "coach".into()] },
            opener_agent_id: None,
            opener_line: None,
            title: None,
        };
        let session = GroupChatSession::new(cfg, resources(provider)).unwrap();
        let obs = Arc::new(RecordingObserver::new());
        session.run_task("用户回答", obs.as_ref() as &dyn GroupChatObserver).await.unwrap();

        let conv = session.conversation().await;
        // user msg + 2 assistant msgs
        assert_eq!(conv.messages.iter().filter(|m| m.role == Role::User).count(), 1);
        assert_eq!(conv.messages.iter().filter(|m| m.role == Role::Assistant).count(), 2);
        // speakers tagged
        let speakers: Vec<&str> = conv.messages.iter()
            .filter_map(|m| m.metadata.get("speaker").map(|s| s.as_str()))
            .collect();
        assert_eq!(speakers, vec!["user", "interviewer", "coach"]);

        // observer saw both speakers' chunks
        let chunks = obs.chunks.lock().unwrap().clone();
        assert!(chunks.iter().any(|(s, _)| s == "interviewer"));
        assert!(chunks.iter().any(|(s, _)| s == "coach"));
    }

    #[tokio::test]
    async fn opener_runs_before_user_and_is_tagged() {
        let provider = Arc::new(MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("开场白"),
        ]));
        let cfg = GroupChatConfig {
            members: vec![
                GroupChatMemberSpec { id: "interviewer".into(), name: "面试官".into(), system_prompt: "你是面试官".into() },
            ],
            turn_policy: TurnPolicy::RoundRobin,
            opener_agent_id: Some("interviewer".into()),
            opener_line: Some("开始面试".into()),
            title: None,
        };
        let session = GroupChatSession::new(cfg, resources(provider)).unwrap();
        let obs = Arc::new(RecordingObserver::new());
        session.start(obs.as_ref() as &dyn GroupChatObserver).await.unwrap();
        let conv = session.conversation().await;
        // opener only — no user message yet
        assert!(conv.messages.iter().all(|m| m.role != Role::User));
        assert_eq!(conv.messages.iter().filter(|m| m.role == Role::Assistant).count(), 1);
        assert_eq!(
            conv.messages.iter().filter_map(|m| m.metadata.get("speaker").map(|s| s.as_str())).collect::<Vec<_>>(),
            vec!["interviewer"]
        );
    }

    #[test]
    fn rejects_empty_members_and_unknown_references() {
        let provider = Arc::new(MockProvider::always_answers("x"));
        let bad = GroupChatConfig {
            members: vec![],
            turn_policy: TurnPolicy::RoundRobin,
            opener_agent_id: None, opener_line: None,
            title: None,
        };
        assert!(GroupChatSession::new(bad, resources(provider.clone())).is_err());

        let bad2 = GroupChatConfig {
            members: vec![GroupChatMemberSpec { id: "a".into(), name: "A".into(), system_prompt: "x".into() }],
            turn_policy: TurnPolicy::Scripted { order: vec!["ghost".into()] },
            opener_agent_id: None, opener_line: None,
            title: None,
        };
        assert!(GroupChatSession::new(bad2, resources(provider)).is_err());
    }
}
