//! Context assembler — constructs the conversation context for each loop iteration.
//!
//! The context assembler is responsible for:
//! 1. Building the conversation from all available sources (system prompt,
//!    recent turns, tool results, skills, retrieved context)
//! 2. Detecting context-source changes and re-injecting only what changed
//! 3. Ensuring the assembled context fits within the token budget
//!
//! **Context Epoch mode** (inspired by OpenCode):
//! - First iteration: inject the full baseline (every `ContextSource`'s
//!   content, in priority order)
//! - Subsequent iterations: inject only what changed, governed by each
//!   source's `RefreshPolicy` (OnChange / EveryIteration / Periodic /
//!   OnceAtStart)
//! - This saves ~2000-5000 tokens per iteration (50k-250k tokens per session)
//!
//! Environment sensing (git status, file tree, working directory, …) is owned
//! entirely by the `ContextSource` implementations in `oneai-domain` — the
//! assembler does **not** run its own git/filesystem probes. This makes env
//! sensing pluggable, refresh-policy-governed, and composable across
//! DomainPacks, instead of a hardcoded parallel path.

use std::collections::HashMap;
use std::sync::Arc;
// for `writeln!` on String below
use std::fmt::Write as _;

use oneai_core::Conversation;
use oneai_core::error::Result;

use oneai_domain::context_source::ContextSource;

// ─── ContextAssembler ───────────────────────────────────────────────────────

/// Context assembler — constructs conversation context per loop iteration.
///
/// The assembler:
/// 1. Takes the current conversation from LoopState
/// 2. On the first epoch, injects every context source as the baseline
/// 3. On subsequent epochs, injects only sources whose `RefreshPolicy` says
///    they should reappear (changed / every-turn / periodic)
/// 4. Returns the assembled conversation for inference
///
/// This ensures the model always has up-to-date environment information,
/// even when tool outputs don't directly reflect the changes — the env
/// sensing itself lives in the `ContextSource` impls (e.g. `GitStatusSource`
/// is `OnChange`, so a git-status change re-injects the full git block).
pub struct ContextAssembler {
    /// Domain-specific context sources (injected from DomainPack).
    context_sources: Vec<Arc<dyn ContextSource>>,
    /// Cached context content from sources — re-injected on every `assemble()`.
    cached_context: HashMap<String, String>,
}

impl ContextAssembler {
    /// Create a new context assembler.
    pub fn new() -> Self {
        Self {
            context_sources: Vec::new(),
            cached_context: HashMap::new(),
        }
    }

    /// Create a context assembler with domain-specific context sources.
    pub fn with_context_sources(context_sources: Vec<Arc<dyn ContextSource>>) -> Self {
        Self {
            context_sources,
            cached_context: HashMap::new(),
        }
    }

    /// Assemble the context for a loop iteration.
    ///
    /// **Ephemeral re-injection model** (the durable/ephemeral separation):
    /// `state.conversation` is the durable log (system prompt, user task,
    /// assistant replies, tool results) that the loop appends to and persists.
    /// This assembler produces a *fresh, ephemeral* per-turn assembly — the
    /// durable log clone plus every `ContextSource`'s cached content — that the
    /// inference request uses. Because the assembly is rebuilt every turn and
    /// never written back to the durable log, pinned state (env sensing, core
    /// memory, future task anchor) survives context compression by
    /// **re-injection** rather than by hoping the compressor keeps it. The
    /// compressor only ever sees the ephemeral assembly; whatever it summarizes
    /// away is restored next turn.
    ///
    /// `RefreshPolicy` therefore governs only whether `load()` is re-called
    /// (in `refresh_sources`); the cached content is injected on **every**
    /// `assemble()`. The old OnceAtStart/OnChange "skip re-injection"
    /// optimizations only made sense when injections accumulated into the
    /// durable log — under the ephemeral model they would make a source vanish
    /// after the first turn.
    pub fn assemble(&mut self, state: &crate::agent_loop::LoopState) -> Result<Conversation> {
        let mut conversation = state.conversation.clone();

        // Inject every source with non-empty cached content. The epoch/baseline
        // distinction no longer gates *injection* — only `refresh_sources`
        // uses it to decide whether to re-call `load()`. This is what makes
        // the block anti-compression: it reappears every turn regardless of
        // what the compressor did to the prior assembly.
        self.inject_sources(&mut conversation, |_policy, _key| true);

        Ok(conversation)
    }

    /// Inject context-source messages into the conversation, filtered by `predicate`.
    ///
    /// Sources are injected in ascending `priority()` order. The predicate
    /// receives the source's `RefreshPolicy` and key, and decides whether the
    /// source's cached content is injected on this epoch.
    fn inject_sources<F>(&self, conversation: &mut Conversation, predicate: F)
    where
        F: Fn(&oneai_domain::context_source::RefreshPolicy, &str) -> bool,
    {
        if self.context_sources.is_empty() {
            return;
        }
        let mut sources: Vec<&Arc<dyn ContextSource>> = self.context_sources.iter().collect();
        sources.sort_by_key(|s| s.priority());

        for source in sources {
            let policy = source.refresh_policy();
            if !predicate(&policy, source.key()) {
                continue;
            }
            if let Some(content) = self.cached_context.get(source.key()) {
                if !content.is_empty() {
                    let context_msg = format!("[Context: {}] {}", source.key(), content);
                    conversation.add_message(oneai_core::Message::system(context_msg));
                }
            }
        }
    }

    /// Refresh and cache all context sources (async — called from the loop).
    ///
    /// Under the ephemeral re-injection model this simply re-calls `load()` on
    /// every source every turn and updates the cache; the cached content is
    /// then injected by the next `assemble()`. (`RefreshPolicy` is honored by
    /// the source's own `load()` impl — e.g. an OnChange source may return a
    /// cached string if its internal snapshot hasn't changed — so we still
    /// always call it here.)
    pub async fn refresh_sources(&mut self) -> Result<()> {
        for source in &self.context_sources {
            let content = source.load().await?;
            self.cached_context.insert(source.key().to_string(), content);
        }
        Ok(())
    }
}

impl Default for ContextAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the pinned `[Task Anchor]` block injected every iteration.
///
/// The original user task is the most important context to preserve — if it
/// gets compressed, the agent loses sight of what it's working toward. By
/// re-injecting it as an ephemeral pinned block every turn (and mirroring it
/// in `Conversation::metadata["task_anchor"]`, which every compressor copies
/// verbatim), the task survives compression regardless of the trimming
/// strategy. The `metadata` may carry a distilled intent / handoff under
/// `task_intent` if one was captured earlier.
pub fn task_anchor_block(task: &str, metadata: &std::collections::HashMap<String, String>) -> String {
    let intent = metadata.get("task_intent").map(|s| s.as_str()).filter(|s| !s.is_empty());
    if let Some(intent) = intent {
        format!("[Task Anchor] (do not compress — original task)\n原始任务: {}\n意图: {}", task, intent)
    } else {
        format!("[Task Anchor] (do not compress — original task)\n原始任务: {}", task)
    }
}

/// Build the pinned `[Task Anchor]` block from the durable working-state
/// projection. This is the canonical source — `working_state.goal` is the
/// original goal persisted in the cross-session event log, unaffected by
/// compaction or session restart. Falls back to the metadata-based block
/// only when no working state is bound.
pub fn task_anchor_block_from_working_state(ws: &oneai_core::WorkingState) -> String {
    if ws.intent.is_empty() {
        format!(
            "[Task Anchor] (do not compress — original task)\n原始任务: {}",
            ws.goal
        )
    } else {
        format!(
            "[Task Anchor] (do not compress — original task)\n原始任务: {}\n意图: {}",
            ws.goal, ws.intent
        )
    }
}

/// Build the pinned `[Plan & Progress]` block injected every iteration when a
/// live plan exists. The plan lives in `LoopState` (agent-side) and is also
/// persisted to `Conversation::metadata["plan_state"]`, so it survives both
/// compression and session reload; this block just renders the current
/// checklist so the model always knows what's ✅ done / 🔄 in progress /
/// ⏳ pending without re-reading compressed-away turns.
pub fn plan_progress_block(goal: &str, plan: &crate::plan_state::PlanState) -> String {
    format!(
        "[Plan & Progress] (do not compress — live task list)\n目标: {}\n{}",
        goal,
        plan.render_progress()
    )
}

/// Build the pinned `[Plan & Progress]` block from the durable working-state
/// projection — the cross-session source of truth for the task list. Renders
/// steps as ✅ done / 🔄 in progress / ⏳ pending / ✗ failed.
pub fn plan_progress_block_from_working_state(ws: &oneai_core::WorkingState) -> String {
    use oneai_core::StepStatus;
    let mut out = String::from("[Plan & Progress] (do not compress — live task list)\n");
    let _ = writeln!(out, "目标: {}", ws.goal);
    if ws.steps.is_empty() {
        let _ = writeln!(out, "(尚无计划步骤)");
    } else {
        // Stable order by `order` then id.
        let mut steps: Vec<&oneai_core::Step> = ws.steps.iter().collect();
        steps.sort_by(|a, b| a.order.cmp(&b.order).then(a.id.cmp(&b.id)));
        for s in steps {
            let icon = match s.status {
                StepStatus::Pending => "⏳",
                StepStatus::InProgress => "🔄",
                StepStatus::Completed => "✅",
                StepStatus::Failed => "✗",
            };
            let label = s.active_form.as_deref().unwrap_or(s.description.as_str());
            let _ = writeln!(out, "{icon} [{}] {}", s.id, label);
        }
    }
    out
}

/// Build the pinned `[Decisions Made]` block — the durable record of key
/// decisions taken during this task, so the model doesn't re-litigate settled
/// questions across compaction. Empty block is omitted (returns empty string).
pub fn decisions_block(ws: &oneai_core::WorkingState) -> String {
    if ws.decisions.is_empty() {
        return String::new();
    }
    let mut out = String::from("[Decisions Made] (do not compress — settled decisions)\n");
    for d in &ws.decisions {
        let _ = writeln!(out, "• {} → {}", d.question, d.chosen);
        if !d.rationale.is_empty() {
            let _ = writeln!(out, "    理由: {}", d.rationale);
        }
    }
    out
}

/// Build the pinned `[Blockers]` block — open 卡点 impeding progress, so the
/// model always knows what's stuck. Resolved blockers are omitted. Empty
/// block (no open blockers) returns empty string.
pub fn blockers_block(ws: &oneai_core::WorkingState) -> String {
    use oneai_core::BlockerStatus;
    let open: Vec<&oneai_core::Blocker> =
        ws.blockers.iter().filter(|b| b.status == BlockerStatus::Open).collect();
    if open.is_empty() {
        return String::new();
    }
    let mut out = String::from("[Blockers] (do not compress — open obstacles)\n");
    for b in open {
        let _ = writeln!(out, "⚠ {}: {}", b.id, b.description);
    }
    out
}

/// Build a runtime context block appended to the system prompt each session.
///
/// This guarantees the model always knows "today" (so it can reason about
/// recency) and is explicitly told to reach for `web_search` / `web_fetch`
/// when a question is time-sensitive, instead of answering from potentially
/// stale training memory.
///
/// We append this to the system prompt directly (rather than relying solely on
/// the `DateSource` context source) because: (1) the system prompt survives
/// context compression better than an ad-hoc system message, and (2) it also
/// carries the time-sensitive search guidance, which `DateSource` does not.
pub fn runtime_context_block() -> String {
    let now = chrono::Local::now();
    format!(
        "\n\n**Current date and time**: {} ({})\n\
         \n**Time-sensitive questions (IMPORTANT)**: If the user asks about recent \
         events, news, latest releases or library versions, current prices, live data, \
         or any information that may have changed since your training, do NOT answer from \
         memory — your knowledge has a cutoff. Call `web_search` first to discover current \
         sources, then `web_fetch` to read the most promising results, and answer based on \
         what you find. Only answer from your own knowledge when the topic is clearly stable \
         and well within your training cutoff.",
        now.format("%Y-%m-%d %H:%M:%S %:z"),
        now.format("%A"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::LoopState;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// A fixed-content context source for testing.
    struct StubSource {
        key: &'static str,
        content: &'static str,
    }

    #[async_trait]
    impl ContextSource for StubSource {
        fn key(&self) -> &str { self.key }
        async fn load(&self) -> Result<String> { Ok(self.content.to_string()) }
        fn refresh_policy(&self) -> oneai_domain::context_source::RefreshPolicy {
            // OnceAtStart is the policy most affected by the first-epoch bug —
            // before the fix it was never injected at all.
            oneai_domain::context_source::RefreshPolicy::OnceAtStart
        }
    }

    /// A mutable OnChange source — content can be mutated between epochs to
    /// exercise the change-detection path.
    struct MutableStubSource {
        key: &'static str,
        content: Arc<Mutex<String>>,
    }

    #[async_trait]
    impl ContextSource for MutableStubSource {
        fn key(&self) -> &str { self.key }
        async fn load(&self) -> Result<String> {
            Ok(self.content.lock().unwrap().clone())
        }
        fn refresh_policy(&self) -> oneai_domain::context_source::RefreshPolicy {
            oneai_domain::context_source::RefreshPolicy::OnChange
        }
    }

    fn text_of(conv: &Conversation) -> String {
        conv.messages.iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Under the ephemeral re-injection model, every cached source is injected
    /// on **every** `assemble()` — the durable/ephemeral separation means
    /// pinned state survives compression by re-injection, not by the
    /// OnceAtStart/OnChange "skip" optimizations (those only worked when
    /// injections accumulated into the durable log).
    #[tokio::test]
    async fn every_source_reinjected_every_turn_regardless_of_policy() {
        let sources: Vec<Arc<dyn ContextSource>> = vec![
            Arc::new(StubSource { key: "stub", content: "STUB-BASELINE-CONTENT" }),
        ];
        let mut ca = ContextAssembler::with_context_sources(sources);

        // First epoch: refresh caches content, assemble injects it.
        ca.refresh_sources().await.unwrap();
        let state = LoopState::new("do something");
        let conv = ca.assemble(&state).unwrap();
        let text = text_of(&conv);
        assert!(text.contains("[Context: stub]"), "context source missing on first epoch: {text}");
        assert!(text.contains("STUB-BASELINE-CONTENT"), "baseline content missing: {text}");

        // Second epoch: even though the source is OnceAtStart (policy would
        // have skipped re-injection under the old incremental model), it is
        // re-injected under the ephemeral model — otherwise it would vanish
        // after the first turn and the compressor would be free to drop it.
        ca.refresh_sources().await.unwrap();
        let state2 = LoopState::new("next turn");
        let conv2 = ca.assemble(&state2).unwrap();
        let text2 = text_of(&conv2);
        assert!(text2.contains("STUB-BASELINE-CONTENT"),
            "OnceAtStart source must be re-injected every turn (ephemeral model): {text2}");
    }

    /// An OnChange source is re-injected every turn (its content is always
    /// present in the assembly); `load()` is still called each turn so the
    /// source can update its internal snapshot, and a content change shows up
    /// in the next assembly.
    #[tokio::test]
    async fn on_change_source_reinjected_every_turn_with_current_content() {
        let content = Arc::new(Mutex::new("STUB-A".to_string()));
        let sources: Vec<Arc<dyn ContextSource>> = vec![
            Arc::new(MutableStubSource { key: "stub", content: content.clone() }),
        ];
        let mut ca = ContextAssembler::with_context_sources(sources);

        // First epoch: baseline A is injected.
        ca.refresh_sources().await.unwrap();
        let conv = ca.assemble(&LoopState::new("t1")).unwrap();
        assert!(text_of(&conv).contains("STUB-A"), "baseline missing: {}", text_of(&conv));

        // Second epoch, no change: source is still re-injected (same content).
        ca.refresh_sources().await.unwrap();
        let conv2 = ca.assemble(&LoopState::new("t2")).unwrap();
        assert!(text_of(&conv2).contains("STUB-A"),
            "unchanged OnChange source must still be present (ephemeral): {}", text_of(&conv2));

        // Third epoch, content changes: new content is injected.
        *content.lock().unwrap() = "STUB-B".to_string();
        ca.refresh_sources().await.unwrap();
        let conv3 = ca.assemble(&LoopState::new("t3")).unwrap();
        assert!(text_of(&conv3).contains("STUB-B"),
            "changed content not re-injected: {}", text_of(&conv3));
    }

    #[test]
    fn task_anchor_block_renders_task_and_intent() {
        use std::collections::HashMap;
        let mut meta = HashMap::new();
        let block = task_anchor_block("refactor auth", &meta);
        assert!(block.contains("Task Anchor"));
        assert!(block.contains("refactor auth"));

        meta.insert("task_intent".to_string(), "swap to JWT".to_string());
        let block = task_anchor_block("refactor auth", &meta);
        assert!(block.contains("意图"));
        assert!(block.contains("swap to JWT"));
    }

    #[test]
    fn runtime_context_block_has_date_and_search_guidance() {
        let block = runtime_context_block();
        assert!(block.contains("Current date and time"), "block: {block}");
        assert!(block.contains("web_search"), "block should nudge web_search: {block}");
    }
}
