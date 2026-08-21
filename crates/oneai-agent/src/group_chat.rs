//! GroupChatSession — shared-transcript multi-agent conversation primitive.
//!
//! Unlike the agent loop's `delegate` (fan out to N sub-agents → merge into
//! one result), a group chat is a *dialogue*: N persona
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
use oneai_skill::SkillSelector;

use crate::agent_loop::{AgentLoop, AgentLoopConfig, AgentLoopObserver, AgentLoopResult};
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
    /// A moderator member decides the next speaker **one at a time**, then is
    /// re-queried after that speaker finishes — each pick runs over the *live*
    /// transcript so the moderator can react to what members actually said
    /// (e.g. pick someone to rebut a point just made) rather than pre-planning
    /// the whole sequence from a stale snapshot. Returns a member id or
    /// `"user"` to hand back to the human. Stops at `"user"`, an unknown id,
    /// or after `max_turns` member turns (safety bound).
    Moderator {
        moderator_id: String,
        max_turns: usize,
    },
}

/// Optional review-revise loop for a scripted scenario (e.g. writing workshop:
/// writer drafts → editor reviews → writer revises → editor re-reviews → …
/// until the editor approves). After the scripted speaker sequence runs once,
/// the loop repeats the same sequence; after each pass the reviewer's last
/// message is checked for `approve_marker`. The loop stops on approval or
/// after `max_rounds` total passes (the safety cap that prevents infinite
/// revision). The reviewer's persona prompt must instruct it to emit the
/// marker when satisfied. `None` (the default) = single pass, no loop.
#[derive(Debug, Clone)]
pub struct ReviewLoopConfig {
    /// Member id that decides approval (e.g. "editor").
    pub reviewer_id: String,
    /// Substring the reviewer emits when it's satisfied (e.g. "定稿").
    pub approve_marker: String,
    /// Total scripted passes to run at most (1 = no loop, just the initial pass).
    pub max_rounds: usize,
}

/// Engine prompt language for group-chat turn nudges and moderator routing.
///
/// Chinese (`Zh`, the default) preserves the historical engine behavior; `En`
/// emits English nudges so an English-locale scenario drives the LLM in English
/// end-to-end. The reviewer's `approve_marker` and these prompts must share a
/// locale — an English preset pairs `ChatLocale::En` here with an English
/// marker (e.g. `"approved"`), a Chinese preset pairs `Zh` with `"定稿"`.
/// `#[non_exhaustive]` per the v0.2.0 stability commitment (P3-1); future
/// locales fall back to the Zh prompt set.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatLocale {
    #[default]
    Zh,
    En,
}

impl ChatLocale {
    /// Default opener line when `opener_line` is unset.
    fn opener_default(self) -> &'static str {
        match self {
            Self::En => "Start this conversation in your role.",
            _ => "请以你的角色身份开始这场对话。",
        }
    }

    /// First-ever turn nudge — the first speaker responds directly to the user.
    fn first_ever(self, member_id: &str) -> String {
        match self {
            Self::En => format!("(The user just sent a message; respond as {member_id}.)"),
            _ => format!("（用户刚发来消息，请以 {} 的身份作出回应。）", member_id),
        }
    }

    /// Reviewer re-review nudge under a review-revise loop. Tells the reviewer
    /// to emit `approve_marker` when the draft is ready to finalize.
    fn reviewer_re_review(self, n: usize, approve_marker: &str) -> String {
        match self {
            Self::En => format!(
                "(Now in review round {n}. Re-review the writer's latest revision. \
                 If it has reached a quality ready to finalize, include \"{approve_marker}\" \
                 in your reply to signal approval; otherwise give specific, actionable \
                 revision feedback.)"
            ),
            _ => format!(
                "（已进入第{n}轮复审。请再次审阅写手本轮的修改。若已达到可定稿的质量，请在回复中包含「{}」以示通过；否则给出具体、可执行的修改意见。）",
                approve_marker
            ),
        }
    }

    /// Writer revise nudge under a review-revise loop.
    fn writer_revise(self, n: usize) -> String {
        match self {
            Self::En => format!(
                "(Now in revision round {n}. Revise your draft per the editor's previous \
                 feedback and output the complete draft.)"
            ),
            _ => format!(
                "（已进入第{n}轮修改。请根据编辑上一轮的修改意见修改你的稿件，并输出完整稿件。）"
            ),
        }
    }

    /// Continue-the-dialogue nudge for a non-first, non-review turn.
    fn continue_turn(self, member_id: &str) -> String {
        match self {
            Self::En => format!(
                "(It's your turn to speak ({member_id}); continue the conversation in context.)"
            ),
            _ => format!("（现在轮到你（{member_id}）发言，请结合上文继续对话。）"),
        }
    }

    /// Moderator routing task — pick the next speaker id (or `"user"`).
    fn moderator_pick_prompt(self, member_ids: &[String]) -> String {
        match self {
            Self::En => format!(
                "You are the moderator of this conversation. Based on the current progress, \
                 choose the next role to speak.\nAvailable roles: {} or \"user\" \
                 (hand the floor back to the user).\nReply with only that role's id and nothing else.",
                member_ids.join(", ")
            ),
            _ => format!(
                "你是这场对话的主持人。根据当前对话进展，选择下一位应该发言的角色。\n可选角色: {} 或 \"user\"（把发言权交还给用户）。\n只回复该角色的 id，不要回复其他内容。",
                member_ids.join(", ")
            ),
        }
    }
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
    /// Optional review-revise loop (see [`ReviewLoopConfig`]). When set, the
    /// scripted speaker sequence repeats up to `max_rounds` until the reviewer
    /// emits `approve_marker`. `None` = single pass (default behavior).
    pub review_loop: Option<ReviewLoopConfig>,
    /// Engine prompt language for turn nudges / moderator routing. Defaults to
    /// [`ChatLocale::Zh`] (preserves historical behavior). Set `En` for an
    /// English-locale scenario so the LLM is nudged in English end-to-end —
    /// pairs with an English `approve_marker` on the review loop.
    pub locale: ChatLocale,
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
            TurnPolicy::Moderator { moderator_id, .. } if !ids.contains(moderator_id.as_str()) => {
                return Err(OneAIError::Config(format!(
                    "moderator '{moderator_id}' is not a member"
                )));
            }
            _ => {}
        }
        if let Some(op) = &config.opener_agent_id {
            if !ids.contains(op.as_str()) {
                return Err(OneAIError::Config(format!("opener '{op}' is not a member")));
            }
        }

        let mut loops = HashMap::new();
        for m in &config.members {
            let provider =
                resources.providers.get(&m.id).cloned().ok_or_else(|| {
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
            .unwrap_or_else(|| self.config.locale.opener_default().to_string());
        self.run_member(&opener_id, task, observer).await?;
        self.persist().await;
        Ok(())
    }

    /// Append the user's message, then run speakers per the turn policy until
    /// it's the user's turn again. Emits speaker-labeled events through
    /// `observer`.
    pub async fn run_task(&self, user_input: &str, observer: &dyn GroupChatObserver) -> Result<()> {
        if self.interrupt_flag.load(Ordering::Relaxed) {
            self.interrupt_flag.store(false, Ordering::Relaxed);
        }

        // 1. Append the user's message to the shared transcript.
        {
            let mut conv = self.conversation.lock().await;
            let mut msg = Message::user(user_input.to_string());
            msg.metadata
                .insert("speaker".to_string(), "user".to_string());
            conv.add_message(msg);
        }

        // 2. Run speakers per the turn policy. Moderator is driven dynamically
        //    (pick one speaker → run it → re-pick from the *live* transcript so
        //    the moderator can react to what was just said); Scripted /
        //    RoundRobin use a fixed sequence, optionally review-looped.
        let turn_policy = self.turn_policy.lock().await.clone();
        match turn_policy {
            TurnPolicy::Moderator {
                moderator_id,
                max_turns,
            } => {
                self.run_moderator_round(&moderator_id, max_turns, observer)
                    .await?;
            }
            _ => {
                let speakers = self.speakers_for_round().await?;
                let review_loop = self.config.review_loop.clone();
                let max_rounds = review_loop.as_ref().map(|r| r.max_rounds).unwrap_or(1);

                // Run the sequence, optionally repeating (review-revise loop)
                // until the reviewer approves or max_rounds is reached.
                let mut first_ever = true;
                for round in 0..max_rounds {
                    for member_id in &speakers {
                        if self.interrupt_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        // First speaker of the very first round responds
                        // directly to the user input (already in the
                        // transcript as the last user message). Subsequent
                        // turns get a role-nudge so they continue rather than
                        // re-answering — and, under a review loop, a
                        // revision/re-review nudge so the writer edits per
                        // feedback.
                        let task = self.turn_nudge(member_id, round, first_ever, &review_loop);
                        first_ever = false;
                        match self.run_member(member_id, task, observer).await {
                            Ok(()) => {}
                            Err(OneAIError::Other(msg)) if msg.contains("interrupted") => break,
                            Err(e) => return Err(e),
                        }
                    }
                    if self.interrupt_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Some(r) = &review_loop {
                        if self.reviewer_approved(r).await {
                            break;
                        }
                    }
                }
            }
        }

        self.persist().await;
        Ok(())
    }

    /// Moderator-driven dynamic round: pick one speaker → run it → re-pick
    /// with the *live* transcript (so the moderator reacts to what was just
    /// said) → repeat. Stops when the moderator returns `"user"` or an unknown
    /// id, or after `max_turns` member turns (safety bound). Unlike the
    /// scripted / round-robin path, the moderator never pre-plans the whole
    /// sequence — each pick is based on the actual conversation progress.
    async fn run_moderator_round(
        &self,
        moderator_id: &str,
        max_turns: usize,
        observer: &dyn GroupChatObserver,
    ) -> Result<()> {
        let mut first_ever = true;
        for _ in 0..max_turns {
            if self.interrupt_flag.load(Ordering::Relaxed) {
                break;
            }
            // Snapshot the live transcript so the moderator sees what the last
            // speaker actually said (not a pre-conversation snapshot). The
            // guard is dropped before moderator_pick / run_member so we never
            // hold the conversation lock across a member's agent-loop run.
            let transcript = {
                let guard = self.conversation.lock().await;
                guard.clone()
            };
            let next = self.moderator_pick(moderator_id, &transcript).await?;
            if next == "user" || next.is_empty() {
                break;
            }
            if !self.config.members.iter().any(|m| m.id == next) {
                // Unknown pick — stop to avoid a runaway loop.
                break;
            }
            let task = self.turn_nudge(&next, 0, first_ever, &None);
            first_ever = false;
            match self.run_member(&next, task, observer).await {
                Ok(()) => {}
                Err(OneAIError::Other(msg)) if msg.contains("interrupted") => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Build the task nudge handed to a member for its turn. The first speaker
    /// of the first round answers the user; later turns continue the dialogue;
    /// and under a review loop the reviewer is told to emit `approve_marker`
    /// when satisfied while other members revise per the reviewer's feedback.
    fn turn_nudge(
        &self,
        member_id: &str,
        round: usize,
        first_ever: bool,
        review_loop: &Option<ReviewLoopConfig>,
    ) -> String {
        let loc = self.config.locale;
        if first_ever {
            return loc.first_ever(member_id);
        }
        if let Some(r) = review_loop {
            let n = round + 1;
            if member_id == r.reviewer_id {
                loc.reviewer_re_review(n, &r.approve_marker)
            } else {
                loc.writer_revise(n)
            }
        } else {
            loc.continue_turn(member_id)
        }
    }

    /// Whether the reviewer's latest turn in the shared transcript contains the
    /// approval marker (case-sensitive substring match). Drives the
    /// review-revise loop's termination.
    async fn reviewer_approved(&self, r: &ReviewLoopConfig) -> bool {
        let conv = self.conversation.lock().await;
        let last = conv.messages.iter().rev().find(|m| {
            m.metadata
                .get("speaker")
                .map(|s| s == &r.reviewer_id)
                .unwrap_or(false)
        });
        match last {
            Some(m) => m.text_content().contains(&r.approve_marker),
            None => false,
        }
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
        let member_loop = self
            .loops
            .get(member_id)
            .ok_or_else(|| OneAIError::Config(format!("unknown member '{member_id}'")))?;

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
            // Moderator is driven dynamically by `run_moderator_round` (pick →
            // run → re-pick from the *live* transcript), NOT by a precomputed
            // sequence. This arm is unreachable from `run_task` (which branches
            // to `run_moderator_round` for the Moderator policy); return empty
            // so a future caller can't get a stale single-snapshot list.
            TurnPolicy::Moderator { .. } => Ok(Vec::new()),
        }
    }

    /// Ask the moderator member to choose the next speaker.
    async fn moderator_pick(
        &self,
        moderator_id: &str,
        transcript: &Conversation,
    ) -> Result<String> {
        let member_loop = self
            .loops
            .get(moderator_id)
            .ok_or_else(|| OneAIError::Config(format!("moderator '{moderator_id}' not found")))?;
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
        let task = self.config.locale.moderator_pick_prompt(&member_ids);
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
    fn on_delegate(&self, _: &str, _: &str, _: &crate::sub_agent::SubAgentKind) {}
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
        token_budget: None, // Persona turns are bounded by hard_max_iterations.
        inject_skills: false,
        usage_tracker: None,
        rate_limiter: None,
        circuit_breaker: None,
        token_counter: None,
        context_manager: None, // Group-chat members are bounded by hard_max_iterations.
        structured_output: None,
        constrained_output_policy: oneai_core::ConstrainedOutputPolicy::Auto,
        trace_context: None,
        #[cfg(feature = "otel")]
        metrics_provider: None,
        plan_mode: false,
        prompt_cache_policy: oneai_core::PromptCachePolicy::Auto,
        reflection_cadence: None,
    };
    let context_assembler = ContextAssembler::new();
    let stream_parser = IncrementalStreamParser::new();
    let budget = TokenBudget::new(100_000);
    // Group-chat members are conversational personas, NOT tool executors
    // (the shared `resources.tools` map is deliberately ignored here). Giving a
    // member the full tool set let a confused persona — e.g. the debate
    // moderator, whose speaker-selector persona clashed with its opening-line
    // task — loop on tool calls (web_search, …), each toolCall/toolResult a
    // non-hot event flushed straight onto the UI thread, flooding it into a
    // persistent beachball ("主持人开场还没输出完就卡住了"). An empty tool map
    // means the LLM is told there are no tools, so it can't tool-loop; the
    // design comment above already states members are "conversational, not
    // tool-heavy executors". The engine's own group-chat tests run with an
    // empty tools map, so this matches the tested path. If a future scenario
    // genuinely needs a tool-using member, opt that in explicitly.
    let no_tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    AgentLoop::new(
        provider,
        no_tools,
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
        fn new() -> Self {
            Self {
                chunks: Mutex::new(vec![]),
                current: Mutex::new(String::new()),
            }
        }
    }
    impl AgentLoopObserver for RecordingObserver {
        fn on_iteration_start(&self, _: usize, _: crate::agent_loop::ParadigmKind) {}
        fn on_stream_chunk(&self, t: &str) {
            let cur = self.current.lock().unwrap().clone();
            self.chunks
                .lock()
                .unwrap()
                .push((cur.clone(), t.to_string()));
        }
        fn on_direct_answer(&self, _: &str) {}
        fn on_tool_calls(&self, _: &[crate::agent_loop::ToolCallRequest]) {}
        fn on_tool_result(&self, _: &str, _: &str, _: &oneai_core::ToolOutput) {}
        fn on_delegate(&self, _: &str, _: &str, _: &crate::sub_agent::SubAgentKind) {}
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
        providers.insert("writer".to_string(), provider.clone());
        providers.insert("editor".to_string(), provider.clone());
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
                GroupChatMemberSpec {
                    id: "interviewer".into(),
                    name: "面试官".into(),
                    system_prompt: "你是面试官".into(),
                },
                GroupChatMemberSpec {
                    id: "coach".into(),
                    name: "指导员".into(),
                    system_prompt: "你是指导员".into(),
                },
            ],
            turn_policy: TurnPolicy::Scripted {
                order: vec!["interviewer".into(), "coach".into()],
            },
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: None,
            locale: ChatLocale::Zh,
        };
        let session = GroupChatSession::new(cfg, resources(provider)).unwrap();
        let obs = Arc::new(RecordingObserver::new());
        session
            .run_task("用户回答", obs.as_ref() as &dyn GroupChatObserver)
            .await
            .unwrap();

        let conv = session.conversation().await;
        // user msg + 2 assistant msgs
        assert_eq!(
            conv.messages
                .iter()
                .filter(|m| m.role == Role::User)
                .count(),
            1
        );
        assert_eq!(
            conv.messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count(),
            2
        );
        // speakers tagged
        let speakers: Vec<&str> = conv
            .messages
            .iter()
            .filter_map(|m| m.metadata.get("speaker").map(|s| s.as_str()))
            .collect();
        assert_eq!(speakers, vec!["user", "interviewer", "coach"]);

        // observer saw both speakers' chunks
        let chunks = obs.chunks.lock().unwrap().clone();
        assert!(chunks.iter().any(|(s, _)| s == "interviewer"));
        assert!(chunks.iter().any(|(s, _)| s == "coach"));
    }

    /// A real mock-provider group round driven through `GroupChatBusObserver`
    /// over an `InProcessBus` emits `SpeakerTurn` + speaker-tagged fragments —
    /// but, by design, NO `TurnComplete` during the round (the observer no-ops
    /// `on_complete` so N members don't each emit one). This is the gap the
    /// sidecar/c_facade runtimes fill by emitting a single round-level
    /// `TurnComplete` after `run_task` returns (an out-of-process frontend
    /// can't observe the `await` returning, so it needs that yield to clear
    /// `running`). Guards that premise: if the observer ever starts emitting a
    /// per-round `TurnComplete`, the duplicate would surface to frontends.
    #[tokio::test]
    async fn bus_observer_round_emits_speakers_but_no_turn_complete() {
        use crate::GroupChatBusObserver;
        use oneai_bus::{EngineBus, EngineYield, InProcessBus};

        let provider = Arc::new(MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("面试官的问题"),
            ScriptedResponse::direct_answer("指导员的点评"),
        ]));
        let cfg = GroupChatConfig {
            members: vec![
                GroupChatMemberSpec {
                    id: "interviewer".into(),
                    name: "面试官".into(),
                    system_prompt: "你是面试官".into(),
                },
                GroupChatMemberSpec {
                    id: "coach".into(),
                    name: "指导员".into(),
                    system_prompt: "你是指导员".into(),
                },
            ],
            turn_policy: TurnPolicy::Scripted {
                order: vec!["interviewer".into(), "coach".into()],
            },
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: None,
            locale: ChatLocale::Zh,
        };
        let session = GroupChatSession::new(cfg, resources(provider)).unwrap();
        let bus: Arc<InProcessBus> = Arc::new(InProcessBus::default());
        let mut rx = bus.subscribe_yields();
        let obs = GroupChatBusObserver::new(bus.clone() as Arc<dyn EngineBus>, "t1");
        session
            .run_task("用户回答", &obs as &dyn GroupChatObserver)
            .await
            .unwrap();

        // Drain every yield the round emitted.
        let mut yields = Vec::new();
        while let Ok(y) = rx.try_recv() {
            yields.push(y);
        }
        // At least one SpeakerTurn per member, tagged with that member.
        let speakers: Vec<String> = yields
            .iter()
            .filter_map(|y| match y {
                EngineYield::SpeakerTurn { speaker, .. } => Some(speaker.clone()),
                _ => None,
            })
            .collect();
        assert!(speakers.contains(&"interviewer".to_string()));
        assert!(speakers.contains(&"coach".to_string()));
        // Fragments carry the speaking member's id (not None — this is group).
        let tagged = yields.iter().any(|y| {
            matches!(y,
            EngineYield::StreamChunk { speaker: Some(s), .. }
            | EngineYield::DirectAnswer { speaker: Some(s), .. }
                if s == "interviewer" || s == "coach")
        });
        assert!(tagged, "expected speaker-tagged fragments");
        // The design premise: no TurnComplete during the round.
        assert!(
            !yields
                .iter()
                .any(|y| matches!(y, EngineYield::TurnComplete { .. })),
            "group observer must not emit TurnComplete (round-level emit is the runtime's job)"
        );
    }

    #[tokio::test]
    async fn opener_runs_before_user_and_is_tagged() {
        let provider = Arc::new(MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("开场白"),
        ]));
        let cfg = GroupChatConfig {
            members: vec![GroupChatMemberSpec {
                id: "interviewer".into(),
                name: "面试官".into(),
                system_prompt: "你是面试官".into(),
            }],
            turn_policy: TurnPolicy::RoundRobin,
            opener_agent_id: Some("interviewer".into()),
            opener_line: Some("开始面试".into()),
            title: None,
            review_loop: None,
            locale: ChatLocale::Zh,
        };
        let session = GroupChatSession::new(cfg, resources(provider)).unwrap();
        let obs = Arc::new(RecordingObserver::new());
        session
            .start(obs.as_ref() as &dyn GroupChatObserver)
            .await
            .unwrap();
        let conv = session.conversation().await;
        // opener only — no user message yet
        assert!(conv.messages.iter().all(|m| m.role != Role::User));
        assert_eq!(
            conv.messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count(),
            1
        );
        assert_eq!(
            conv.messages
                .iter()
                .filter_map(|m| m.metadata.get("speaker").map(|s| s.as_str()))
                .collect::<Vec<_>>(),
            vec!["interviewer"]
        );
    }

    #[test]
    fn rejects_empty_members_and_unknown_references() {
        let provider = Arc::new(MockProvider::always_answers("x"));
        let bad = GroupChatConfig {
            members: vec![],
            turn_policy: TurnPolicy::RoundRobin,
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: None,
            locale: ChatLocale::Zh,
        };
        assert!(GroupChatSession::new(bad, resources(provider.clone())).is_err());

        let bad2 = GroupChatConfig {
            members: vec![GroupChatMemberSpec {
                id: "a".into(),
                name: "A".into(),
                system_prompt: "x".into(),
            }],
            turn_policy: TurnPolicy::Scripted {
                order: vec!["ghost".into()],
            },
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: None,
            locale: ChatLocale::Zh,
        };
        assert!(GroupChatSession::new(bad2, resources(provider)).is_err());
    }

    #[tokio::test]
    async fn review_loop_runs_until_marker_then_stops() {
        // Writing workshop: writer drafts → editor reviews (no marker) →
        // writer revises → editor re-reviews WITH marker → loop stops at 2.
        let provider = Arc::new(MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("初稿"), // writer round 0
            ScriptedResponse::direct_answer("需修改第一段"), // editor round 0 (no marker)
            ScriptedResponse::direct_answer("修改稿"), // writer round 1
            ScriptedResponse::direct_answer("很好，定稿"), // editor round 1 (marker)
            // Safety: if the loop ignored the marker it would call this 5th
            // response and keep going.
            ScriptedResponse::direct_answer("不该出现的第三轮"),
        ]));
        let cfg = GroupChatConfig {
            members: vec![
                GroupChatMemberSpec {
                    id: "writer".into(),
                    name: "写手".into(),
                    system_prompt: "你是写手".into(),
                },
                GroupChatMemberSpec {
                    id: "editor".into(),
                    name: "编辑".into(),
                    system_prompt: "你是编辑".into(),
                },
            ],
            turn_policy: TurnPolicy::Scripted {
                order: vec!["writer".into(), "editor".into()],
            },
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: Some(ReviewLoopConfig {
                reviewer_id: "editor".into(),
                approve_marker: "定稿".into(),
                max_rounds: 3,
            }),
            locale: ChatLocale::Zh,
        };
        let session = GroupChatSession::new(cfg, resources(provider)).unwrap();
        let obs = Arc::new(RecordingObserver::new());
        session
            .run_task("写一篇散文", obs.as_ref() as &dyn GroupChatObserver)
            .await
            .unwrap();

        let conv = session.conversation().await;
        // 4 assistant turns (2 rounds × 2 members), not 6.
        let assistant = conv
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        assert_eq!(assistant, 4, "loop must stop at the approval marker");
        // Last speaker is the editor, whose answer carries the marker.
        let last = conv
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .unwrap();
        assert_eq!(
            last.metadata.get("speaker").map(|s| s.as_str()),
            Some("editor")
        );
        assert!(last.text_content().contains("定稿"));
    }

    #[tokio::test]
    async fn review_loop_caps_at_max_rounds_without_marker() {
        // Editor never emits the marker → loop must hit the max_rounds cap (2)
        // and stop, not run forever.
        let provider = Arc::new(MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("初稿"),
            ScriptedResponse::direct_answer("再改改"),
            ScriptedResponse::direct_answer("修改稿"),
            ScriptedResponse::direct_answer("还是不行"),
        ]));
        let cfg = GroupChatConfig {
            members: vec![
                GroupChatMemberSpec {
                    id: "writer".into(),
                    name: "写手".into(),
                    system_prompt: "你是写手".into(),
                },
                GroupChatMemberSpec {
                    id: "editor".into(),
                    name: "编辑".into(),
                    system_prompt: "你是编辑".into(),
                },
            ],
            turn_policy: TurnPolicy::Scripted {
                order: vec!["writer".into(), "editor".into()],
            },
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: Some(ReviewLoopConfig {
                reviewer_id: "editor".into(),
                approve_marker: "定稿".into(),
                max_rounds: 2,
            }),
            locale: ChatLocale::Zh,
        };
        let session = GroupChatSession::new(cfg, resources(provider)).unwrap();
        let obs = Arc::new(RecordingObserver::new());
        session
            .run_task("写一篇散文", obs.as_ref() as &dyn GroupChatObserver)
            .await
            .unwrap();

        let conv = session.conversation().await;
        // 2 rounds × 2 members = 4 assistant turns; cap held.
        assert_eq!(
            conv.messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count(),
            4,
            "loop must cap at max_rounds without the marker"
        );
    }

    // ─── Moderator dynamic re-pick ──────────────────────────────────────────

    /// A content-aware provider that proves the moderator re-reads the *live*
    /// transcript after each turn. On a moderator pick call (task contains
    /// "主持人") it counts how many non-moderator members have already spoken
    /// and returns "a", then "b", then "user" (hand back to human). Under the
    /// OLD pre-plan code (one snapshot taken before any member spoke), every
    /// pick would see 0 spoken and return "a" forever → "b" never speaks. So
    /// "b" speaking at all is the proof the moderator now reacts to the live
    /// conversation instead of a stale snapshot.
    struct ReactingModeratorProvider {
        config: oneai_core::ModelConfig,
    }

    impl ReactingModeratorProvider {
        fn new() -> Self {
            Self {
                config: oneai_core::ModelConfig::openai("k".into(), "reacting-moderator".into()),
            }
        }

        fn respond(&self, req: &oneai_core::InferenceRequest) -> String {
            let last_user = req
                .conversation
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(|m| m.text_content())
                .unwrap_or_default();
            if last_user.contains("主持人") {
                let spoken = req
                    .conversation
                    .messages
                    .iter()
                    .filter(|m| m.role == Role::Assistant)
                    .filter(|m| {
                        matches!(
                            m.metadata.get("speaker").map(|s| s.as_str()),
                            Some("a") | Some("b")
                        )
                    })
                    .count();
                match spoken {
                    0 => "a".into(),
                    1 => "b".into(),
                    _ => "user".into(),
                }
            } else {
                // A member's speaking turn — say something.
                "好的，我的看法是…".into()
            }
        }

        fn text_response(&self, text: String) -> oneai_core::InferenceResponse {
            oneai_core::InferenceResponse {
                message: Message::assistant(text),
                usage: oneai_core::TokenUsage::default(),
                model: "reacting-moderator".into(),
                metadata: HashMap::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ReactingModeratorProvider {
        async fn infer(
            &self,
            req: oneai_core::InferenceRequest,
        ) -> std::result::Result<oneai_core::InferenceResponse, OneAIError> {
            Ok(self.text_response(self.respond(&req)))
        }

        async fn infer_stream(
            &self,
            req: oneai_core::InferenceRequest,
        ) -> std::result::Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = oneai_core::InferenceStreamChunk> + Send>>,
            OneAIError,
        > {
            let text = self.respond(&req);
            let model = self
                .config
                .model_name
                .clone()
                .unwrap_or_else(|| "reacting-moderator".into());
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tokio::spawn(async move {
                tx.send(oneai_core::InferenceStreamChunk {
                    content: vec![oneai_core::ContentBlock::Text { text }],
                    is_final: false,
                    usage: None,
                    model: Some(model.clone()),
                })
                .await
                .ok();
                tx.send(oneai_core::InferenceStreamChunk {
                    content: vec![],
                    is_final: true,
                    usage: Some(oneai_core::TokenUsage::default()),
                    model: Some(model),
                })
                .await
                .ok();
            });
            Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }

        fn capabilities(&self) -> oneai_core::ModelCapability {
            oneai_core::ModelCapability::claude_class()
        }

        fn config(&self) -> &oneai_core::ModelConfig {
            &self.config
        }
    }

    fn moderator_resources(provider: Arc<dyn LlmProvider>) -> GroupChatResources {
        let mut providers = HashMap::new();
        for id in ["host", "a", "b"] {
            providers.insert(id.to_string(), provider.clone());
        }
        GroupChatResources {
            providers,
            tools: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            parser: Arc::new(oneai_parser::ThreeLayerParser::new()),
            interaction_gate: Arc::new(oneai_tool::NoopInteractionGate),
        }
    }

    #[tokio::test]
    async fn moderator_re_picks_from_live_transcript() {
        // host = moderator; a, b = speakers. The moderator picks "a" first,
        // then — only after seeing a's answer in the live transcript — "b",
        // then "user" (stop). max_turns = 3 is a safety cap; the loop must
        // stop at "user" before hitting it, with a then b each speaking once.
        let provider = Arc::new(ReactingModeratorProvider::new());
        let cfg = GroupChatConfig {
            members: vec![
                GroupChatMemberSpec {
                    id: "host".into(),
                    name: "主持人".into(),
                    system_prompt: "你是主持人".into(),
                },
                GroupChatMemberSpec {
                    id: "a".into(),
                    name: "A".into(),
                    system_prompt: "你是A".into(),
                },
                GroupChatMemberSpec {
                    id: "b".into(),
                    name: "B".into(),
                    system_prompt: "你是B".into(),
                },
            ],
            turn_policy: TurnPolicy::Moderator {
                moderator_id: "host".into(),
                max_turns: 3,
            },
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: None,
            locale: ChatLocale::Zh,
        };
        let session = GroupChatSession::new(cfg, moderator_resources(provider)).unwrap();
        let obs = Arc::new(RecordingObserver::new());
        session
            .run_task("开始讨论", obs.as_ref() as &dyn GroupChatObserver)
            .await
            .unwrap();

        let conv = session.conversation().await;
        let speakers: Vec<&str> = conv
            .messages
            .iter()
            .filter_map(|m| m.metadata.get("speaker").map(|s| s.as_str()))
            .collect();
        // user + a + b. Crucially b spoke — the moderator only picks b after
        // seeing a's answer, which the old single-snapshot pre-plan couldn't.
        assert_eq!(speakers, vec!["user", "a", "b"]);
        // Each non-moderator member spoke exactly once (not "a" 3× up to the
        // max_turns cap, which is what the stale-snapshot code would do).
        let a_count = conv
            .messages
            .iter()
            .filter(|m| m.metadata.get("speaker").map(|s| s == "a").unwrap_or(false))
            .count();
        let b_count = conv
            .messages
            .iter()
            .filter(|m| m.metadata.get("speaker").map(|s| s == "b").unwrap_or(false))
            .count();
        assert_eq!(a_count, 1, "a speaks once, not repeatedly up to max_turns");
        assert_eq!(
            b_count, 1,
            "b speaks once — proof the moderator reacted to a's turn"
        );
    }

    // ─── ChatLocale prompt localization ──────────────────────────────────────

    /// Pin both locales' prompt strings so an English-locale scenario gets
    /// English nudges end-to-end, and the Chinese default is unchanged.
    #[test]
    fn locale_prompt_strings_localized() {
        // Opener default.
        assert!(ChatLocale::Zh.opener_default().contains("请以你的角色身份"));
        assert!(ChatLocale::En
            .opener_default()
            .contains("Start this conversation"));

        // First-ever turn nudge (member id interpolated).
        let fe_zh = ChatLocale::Zh.first_ever("interviewer");
        let fe_en = ChatLocale::En.first_ever("interviewer");
        assert!(fe_zh.contains("请以 interviewer 的身份"));
        assert!(fe_en.contains("respond as interviewer"));

        // Continue-the-dialogue nudge.
        let ct_zh = ChatLocale::Zh.continue_turn("coach");
        let ct_en = ChatLocale::En.continue_turn("coach");
        assert!(ct_zh.contains("现在轮到你"));
        assert!(ct_en.contains("your turn to speak"));

        // Reviewer re-review — marker interpolated, must appear verbatim in both
        // (the reviewer's reply is matched by substring on approve_marker).
        let rr_zh = ChatLocale::Zh.reviewer_re_review(2, "定稿");
        let rr_en = ChatLocale::En.reviewer_re_review(2, "approved");
        assert!(rr_zh.contains("第2轮复审") && rr_zh.contains("定稿"));
        assert!(rr_en.contains("review round 2") && rr_en.contains("approved"));

        // Writer revise nudge carries the round number.
        assert!(ChatLocale::Zh.writer_revise(2).contains("第2轮修改"));
        assert!(ChatLocale::En.writer_revise(2).contains("revision round 2"));

        // Moderator routing prompt lists the member ids and demands id-only.
        let ids = vec!["a".to_string(), "b".to_string()];
        let mp_zh = ChatLocale::Zh.moderator_pick_prompt(&ids);
        let mp_en = ChatLocale::En.moderator_pick_prompt(&ids);
        assert!(mp_zh.contains("你是这场对话的主持人") && mp_zh.contains("只回复该角色"));
        assert!(
            mp_en.contains("You are the moderator")
                && mp_en.contains("Reply with only that role's id")
                && mp_en.contains("a, b")
        );

        // Default is Zh (preserves historical behavior).
        assert_eq!(ChatLocale::default(), ChatLocale::Zh);
    }

    /// Smoke that `locale: ChatLocale::En` threads through `turn_nudge`
    /// (first-ever + continue nudges) without panicking and still tags speakers
    /// correctly — i.e. the English path is wired, not just the strings.
    #[tokio::test]
    async fn english_locale_scripted_round_threads() {
        let provider = Arc::new(MockProvider::from_script(vec![
            ScriptedResponse::direct_answer("answer one"),
            ScriptedResponse::direct_answer("answer two"),
        ]));
        let cfg = GroupChatConfig {
            members: vec![
                GroupChatMemberSpec {
                    id: "interviewer".into(),
                    name: "Interviewer".into(),
                    system_prompt: "You are the interviewer.".into(),
                },
                GroupChatMemberSpec {
                    id: "coach".into(),
                    name: "Coach".into(),
                    system_prompt: "You are the coach.".into(),
                },
            ],
            turn_policy: TurnPolicy::Scripted {
                order: vec!["interviewer".into(), "coach".into()],
            },
            opener_agent_id: None,
            opener_line: None,
            title: None,
            review_loop: None,
            locale: ChatLocale::En,
        };
        let session = GroupChatSession::new(cfg, resources(provider)).unwrap();
        let obs = Arc::new(RecordingObserver::new());
        session
            .run_task("hello", obs.as_ref() as &dyn GroupChatObserver)
            .await
            .unwrap();

        let conv = session.conversation().await;
        let speakers: Vec<&str> = conv
            .messages
            .iter()
            .filter_map(|m| m.metadata.get("speaker").map(|s| s.as_str()))
            .collect();
        // user + interviewer + coach — the English-nudged turns ran and tagged.
        assert_eq!(speakers, vec!["user", "interviewer", "coach"]);
    }
}
