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
use std::path::Path;
use std::sync::Arc;
// for `writeln!` on String below
use std::fmt::Write as _;

use oneai_core::error::Result;
use oneai_core::Conversation;

use oneai_domain::context_source::{ContextPosition, ContextSource};

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

        // Per-iteration lossless truncation of STALE tool results (gap-analysis
        // follow-up): old large tool outputs (a 16k web page from 3 iterations
        // ago) stop being re-processed every iteration. Only the ephemeral
        // inference copy is trimmed — the durable log keeps full outputs.
        self.truncate_stale_tool_results(&mut conversation);

        Ok(conversation)
    }

    /// Per-iteration lossless truncation of STALE tool results on the ephemeral
    /// inference copy. The durable log keeps full outputs; only the per-request
    /// assembly is trimmed so old large tool results (e.g. a 16k web page from
    /// 3 iterations ago) stop being re-processed every iteration. The last
    /// `KEEP_FULL_RECENT` tool results stay full (the active batch + recent
    /// reference); older ones over `MAX_STALE_TOOL_RESULT_CHARS` are capped to a
    /// snippet + a pointer. Idempotent + no-op on short results / small convs.
    ///
    /// `memory_search` only searches archived facts, not raw tool outputs, so
    /// the pointer tells the model to re-run the tool for the full output (the
    /// durable transcript has it, but it's not in context anymore) — never a
    /// false "use memory_search" promise. Assistant/user/system messages are
    /// never touched (the model's own prior reasoning/output must stay intact).
    fn truncate_stale_tool_results(&self, conversation: &mut Conversation) {
        const MAX_STALE_TOOL_RESULT_CHARS: usize = 2000;
        const KEEP_FULL_RECENT: usize = 4;

        // Indexes of Tool-role messages, in order.
        let tool_idx: Vec<usize> = conversation
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == oneai_core::Role::Tool)
            .map(|(i, _)| i)
            .collect();
        if tool_idx.len() <= KEEP_FULL_RECENT {
            return; // nothing stale — small conversation, leave it alone.
        }
        let stale_count = tool_idx.len() - KEEP_FULL_RECENT;
        for &i in tool_idx.iter().take(stale_count) {
            for block in &mut conversation.messages[i].content {
                if let oneai_core::ContentBlock::ToolResult {
                    call_id: _,
                    content,
                } = block
                {
                    // Count first (releases the immutable borrow) before the
                    // mutable assign below. `chars().take()` keeps UTF-8
                    // boundaries safe.
                    if content.chars().count() > MAX_STALE_TOOL_RESULT_CHARS {
                        let cut: String =
                            content.chars().take(MAX_STALE_TOOL_RESULT_CHARS).collect();
                        *content = format!(
                            "{cut}\n[...truncated — full output is in the session transcript; \
                             re-run the tool to retrieve it in full]"
                        );
                    }
                }
            }
        }
    }

    /// Inject context-source messages into the conversation, filtered by `predicate`.
    ///
    /// Sources are injected in ascending `priority()` order, split by
    /// [`ContextPosition`] into two regions:
    /// - `Prefix` sources are prepended (before the conversation history) — the
    ///   byte-stable, cache-friendly front of the request.
    /// - `Tail` sources are appended (after the history) — the per-turn dynamic
    ///   region whose churn must not invalidate the cached prefix.
    ///
    /// The predicate receives the source's `RefreshPolicy` and key, and decides
    /// whether the source's cached content is injected on this epoch.
    fn inject_sources<F>(&self, conversation: &mut Conversation, predicate: F)
    where
        F: Fn(&oneai_domain::context_source::RefreshPolicy, &str) -> bool,
    {
        if self.context_sources.is_empty() {
            return;
        }
        let mut sources: Vec<&Arc<dyn ContextSource>> = self.context_sources.iter().collect();
        sources.sort_by_key(|s| s.priority());

        let mut prefix: Vec<oneai_core::Message> = Vec::new();
        let mut tail: Vec<oneai_core::Message> = Vec::new();

        for source in sources {
            let policy = source.refresh_policy();
            if !predicate(&policy, source.key()) {
                continue;
            }
            if let Some(content) = self.cached_context.get(source.key()) {
                if !content.is_empty() {
                    let context_msg = oneai_core::Message::system(format!(
                        "[Context: {}] {}",
                        source.key(),
                        content
                    ));
                    match source.position() {
                        ContextPosition::Prefix => prefix.push(context_msg),
                        ContextPosition::Tail => tail.push(context_msg),
                        // `ContextPosition` is #[non_exhaustive] — unknown
                        // positions default to the stable, cache-friendly prefix.
                        _ => prefix.push(context_msg),
                    }
                }
            }
        }

        // Prepend prefix sources (iterated in reverse so the ascending-priority
        // order survives the index-0 inserts), then append the tail sources.
        for msg in prefix.into_iter().rev() {
            conversation.messages.insert(0, msg);
        }
        for msg in tail {
            conversation.add_message(msg);
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
            self.cached_context
                .insert(source.key().to_string(), content);
        }
        Ok(())
    }

    /// Whether any source is path-bound (reads from a project directory).
    ///
    /// Gates advertising the `switch_project` meta-tool to the model — it's
    /// only useful when at least one source would actually rebind, so a
    /// no-domain build (mobile / macOS native) doesn't show a no-op.
    pub fn has_path_bound_sources(&self) -> bool {
        self.context_sources.iter().any(|s| s.is_path_bound())
    }

    /// Re-bind every path-bound source to a different project directory.
    ///
    /// Called when the model invokes the `switch_project` meta-tool (parsed in
    /// `AgentLoop::parse_decision` → `AgentDecision::SwitchProject`). Each
    /// path-bound source updates its interior-mutable directory; the rebound
    /// sources' cached entries are dropped so the next `assemble()` doesn't
    /// inject stale content from the old project — `refresh_sources()` (called
    /// at the top of the next iteration) repopulates them from the new dir.
    ///
    /// Returns the number of sources rebound (0 when the new dir equals the
    /// current one for every source, or no source is path-bound).
    pub fn rebind_project_dir(&mut self, dir: &Path) -> usize {
        let mut rebound = 0usize;
        for source in &self.context_sources {
            if source.rebind_project_dir(dir) {
                self.cached_context.remove(source.key());
                rebound += 1;
            }
        }
        rebound
    }
}

impl Default for ContextAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the sectioned context snapshot for one iteration (issue #40
/// trajectory panel).
///
/// Walks the fully-assembled request conversation and splits it into labeled
/// [`oneai_core::ContextSection`]s: the base system prompt, every
/// `[Context: key]` source block (env sensing, memory recall), the pinned
/// blocks (task anchor / plan / decisions / blockers / skill menu / …), the
/// tool definitions, the latest user message, and a history summary. Each
/// section's content is hash-deduped against `hashes` (keyed by
/// [`oneai_core::ContextKey::cache_key`]) — unchanged sections are emitted
/// with `content: None` so the wire stays small across iterations; the
/// caller clears `hashes` per turn so the first iteration re-sends the full
/// baseline.
pub fn build_context_snapshot(
    iteration: usize,
    model: &str,
    conversation: &Conversation,
    tools: &[oneai_core::ToolDefinition],
    accounting: &oneai_core::ContextAccounting,
    hashes: &mut HashMap<String, u64>,
) -> oneai_core::ContextSnapshot {
    use oneai_core::token_counter::TokenCounter;
    use oneai_core::{ContextKey, ContextSection, ContextSnapshot, Role};
    use std::hash::{Hash, Hasher};

    let counter = oneai_core::token_counter::HeuristicTokenCounter::new();

    // ── Classify messages ──────────────────────────────────────────────
    let mut base_parts: Vec<String> = Vec::new();
    let mut sections: Vec<(ContextKey, String)> = Vec::new();
    let mut latest_user: Option<String> = None;
    let (mut n_user, mut n_assistant, mut n_tool) = (0usize, 0usize, 0usize);

    for msg in &conversation.messages {
        match msg.role {
            Role::System => {
                let text = msg.text_content();
                let key = if let Some(rest) = text.strip_prefix("[Context: ") {
                    let source_key = rest.split(']').next().unwrap_or("").to_string();
                    ContextKey::Context(source_key)
                } else if text.starts_with("[Task Anchor]") {
                    ContextKey::TaskAnchor
                } else if text.starts_with("[Plan & Progress]") {
                    ContextKey::PlanProgress
                } else if text.starts_with("[Decisions Made]") {
                    ContextKey::Decisions
                } else if text.starts_with("[Blockers]") {
                    ContextKey::Blockers
                } else if text.starts_with("# Available skills") {
                    ContextKey::SkillMenu
                } else if text.starts_with("# Active skill") {
                    ContextKey::ActiveSkill
                } else if text.starts_with("# Newly available tools") {
                    ContextKey::NewTools
                } else if text.starts_with("[Background tasks]") {
                    ContextKey::BackgroundTasks
                } else {
                    base_parts.push(text);
                    continue;
                };
                sections.push((key, text));
            }
            Role::User => {
                n_user += 1;
                latest_user = Some(msg.text_content());
            }
            Role::Assistant => n_assistant += 1,
            Role::Tool => n_tool += 1,
            _ => {}
        }
    }

    // ── Assemble the section list (fixed, documented order) ────────────
    let mut ordered: Vec<(ContextKey, String, u64)> = Vec::new();
    if !base_parts.is_empty() {
        let content = base_parts.join("\n\n");
        let tokens = counter.count_tokens(&content, model) as u64;
        ordered.push((ContextKey::BasePrompt, content, tokens));
    }
    for (key, text) in sections {
        let tokens = counter.count_tokens(&text, model) as u64;
        ordered.push((key, text, tokens));
    }
    if !tools.is_empty() {
        // Compact JSON: name + description + schema (what the provider sends).
        let content = serde_json::to_string(tools).unwrap_or_default();
        ordered.push((
            ContextKey::Tools,
            content,
            accounting.tool_call_tokens as u64,
        ));
    }
    if let Some(text) = latest_user.clone() {
        let tokens = counter.count_tokens(&text, model) as u64;
        ordered.push((ContextKey::LatestUser, text, tokens));
    }
    // History summary — everything that isn't a section above. The message
    // counts exclude the latest user message (it has its own section) and
    // system messages (classified). Token share is the residual of the
    // accounting total minus every named section.
    let history_messages = n_user.saturating_sub(1) + n_assistant + n_tool;
    let named_tokens: u64 = ordered.iter().map(|(_, _, t)| *t).sum();
    let history_tokens = (accounting.total_tokens as u64).saturating_sub(named_tokens);
    ordered.push((
        ContextKey::History,
        format!(
            "{history_messages} messages ({n_user} user total, {n_assistant} assistant, {n_tool} tool results)"
        ),
        history_tokens,
    ));

    // ── Hash-dedup content within the turn ─────────────────────────────
    let mut out_sections: Vec<ContextSection> = Vec::with_capacity(ordered.len());
    for (key, content, tokens) in ordered {
        let cache_key = key.cache_key();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        let content_hash = hasher.finish();
        let unchanged = hashes.get(&cache_key) == Some(&content_hash);
        if !unchanged {
            hashes.insert(cache_key, content_hash);
        }
        // Context sources get the actual source key in their label (e.g.
        // "context: git_status") — a generic "context source" label hides which
        // of the N ambient sources a section is (issue #40 follow-up).
        let label = match &key {
            ContextKey::Context(k) => format!("context: {k}"),
            _ => key.label().to_string(),
        };
        out_sections.push(ContextSection {
            key,
            label,
            tokens,
            content_hash,
            content: if unchanged { None } else { Some(content) },
        });
    }

    ContextSnapshot {
        iteration,
        sections: out_sections,
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
pub fn task_anchor_block(
    task: &str,
    metadata: &std::collections::HashMap<String, String>,
) -> String {
    let intent = metadata
        .get("task_intent")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty());
    if let Some(intent) = intent {
        format!(
            "[Task Anchor] (do not compress — original task)\n原始任务: {}\n意图: {}",
            task, intent
        )
    } else {
        format!(
            "[Task Anchor] (do not compress — original task)\n原始任务: {}",
            task
        )
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
    let open: Vec<&oneai_core::Blocker> = ws
        .blockers
        .iter()
        .filter(|b| b.status == BlockerStatus::Open)
        .collect();
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
/// This is the **time-sensitive search guidance** only — it tells the model to
/// reach for `web_search` / `web_fetch` when a question is time-sensitive
/// instead of answering from potentially stale training memory.
///
/// The current date itself is deliberately NOT embedded here (it was
/// previously, as a per-second `Current date and time` line). That timestamp
/// churned the byte-stable base prompt every session and duplicated the
/// `DateSource` context source (which is now date-only and injected in the
/// dynamic tail each turn). The base prompt must stay byte-stable across
/// iterations so the provider's prompt-prefix cache can hold it.
pub fn runtime_context_block() -> String {
    "\n\n**Time-sensitive questions (IMPORTANT)**: If the user asks about recent \
     events, news, latest releases or library versions, current prices, live data, \
     or any information that may have changed since your training, do NOT answer from \
     memory — your knowledge has a cutoff. Call `web_search` first to discover current \
     sources, then `web_fetch` to read the most promising results, and answer based on \
     what you find. Only answer from your own knowledge when the topic is clearly stable \
     and well within your training cutoff."
        .to_string()
}

/// Build the memory-guidance block appended to the system prompt when the
/// active domain registers self-managed memory tools.
///
/// This is the **per-turn model-driven capture** mechanism (mirrors OpenClaw:
/// the system prompt instructs the model to write durable facts to memory when
/// the user shares them, and to recall them on demand). It is distinct from
/// the periodic reflection/consolidation mechanism (`reflection.rs` /
/// compression-coupled `FactExtractor`) — those run in the background; this
/// nudges the model every turn.
///
/// Core memory (facts written via `core_memory_edit`) is persisted to SQLite
/// (user-scoped, cross-session) and reloaded into the always-in-context
/// `[Core Memory]` block at session start, so durable facts survive `/clear`
/// and process restart without a tool call. Archival memory is recalled on
/// demand via `memory_search`.
pub fn memory_guidance_block() -> String {
    "\n\n**Memory (IMPORTANT)**: You have a persistent long-term memory. When \
     the user shares durable information about themselves or the task — name, \
     identity, preferences, decisions, constraints, or any fact worth \
     recalling later — proactively call `core_memory_edit` to store or update \
     it in the same turn you learn it (a fact with the same \
     `subject`+`predicate` is updated in place, so revise rather than \
     duplicate). Do not wait or assume you will remember from context alone. \
     When a question may depend on something from an earlier or previous \
     session that you do not see in context, call `memory_search` to recall \
     it before answering. Core memory (facts you write via \
     `core_memory_edit`) is shown to you every turn; archival memory is \
     recalled on demand."
        .to_string()
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
        fn key(&self) -> &str {
            self.key
        }
        async fn load(&self) -> Result<String> {
            Ok(self.content.to_string())
        }
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
        fn key(&self) -> &str {
            self.key
        }
        async fn load(&self) -> Result<String> {
            Ok(self.content.lock().unwrap().clone())
        }
        fn refresh_policy(&self) -> oneai_domain::context_source::RefreshPolicy {
            oneai_domain::context_source::RefreshPolicy::OnChange
        }
    }

    fn text_of(conv: &Conversation) -> String {
        conv.messages
            .iter()
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
        let sources: Vec<Arc<dyn ContextSource>> = vec![Arc::new(StubSource {
            key: "stub",
            content: "STUB-BASELINE-CONTENT",
        })];
        let mut ca = ContextAssembler::with_context_sources(sources);

        // First epoch: refresh caches content, assemble injects it.
        ca.refresh_sources().await.unwrap();
        let state = LoopState::new("do something");
        let conv = ca.assemble(&state).unwrap();
        let text = text_of(&conv);
        assert!(
            text.contains("[Context: stub]"),
            "context source missing on first epoch: {text}"
        );
        assert!(
            text.contains("STUB-BASELINE-CONTENT"),
            "baseline content missing: {text}"
        );

        // Second epoch: even though the source is OnceAtStart (policy would
        // have skipped re-injection under the old incremental model), it is
        // re-injected under the ephemeral model — otherwise it would vanish
        // after the first turn and the compressor would be free to drop it.
        ca.refresh_sources().await.unwrap();
        let state2 = LoopState::new("next turn");
        let conv2 = ca.assemble(&state2).unwrap();
        let text2 = text_of(&conv2);
        assert!(
            text2.contains("STUB-BASELINE-CONTENT"),
            "OnceAtStart source must be re-injected every turn (ephemeral model): {text2}"
        );
    }

    /// An OnChange source is re-injected every turn (its content is always
    /// present in the assembly); `load()` is still called each turn so the
    /// source can update its internal snapshot, and a content change shows up
    /// in the next assembly.
    #[tokio::test]
    async fn on_change_source_reinjected_every_turn_with_current_content() {
        let content = Arc::new(Mutex::new("STUB-A".to_string()));
        let sources: Vec<Arc<dyn ContextSource>> = vec![Arc::new(MutableStubSource {
            key: "stub",
            content: content.clone(),
        })];
        let mut ca = ContextAssembler::with_context_sources(sources);

        // First epoch: baseline A is injected.
        ca.refresh_sources().await.unwrap();
        let conv = ca.assemble(&LoopState::new("t1")).unwrap();
        assert!(
            text_of(&conv).contains("STUB-A"),
            "baseline missing: {}",
            text_of(&conv)
        );

        // Second epoch, no change: source is still re-injected (same content).
        ca.refresh_sources().await.unwrap();
        let conv2 = ca.assemble(&LoopState::new("t2")).unwrap();
        assert!(
            text_of(&conv2).contains("STUB-A"),
            "unchanged OnChange source must still be present (ephemeral): {}",
            text_of(&conv2)
        );

        // Third epoch, content changes: new content is injected.
        *content.lock().unwrap() = "STUB-B".to_string();
        ca.refresh_sources().await.unwrap();
        let conv3 = ca.assemble(&LoopState::new("t3")).unwrap();
        assert!(
            text_of(&conv3).contains("STUB-B"),
            "changed content not re-injected: {}",
            text_of(&conv3)
        );
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
    fn runtime_context_block_has_search_guidance_and_no_timestamp() {
        let block = runtime_context_block();
        assert!(
            block.contains("web_search"),
            "block should nudge web_search: {block}"
        );
        // The date/time must NOT be embedded in the base prompt — it churns the
        // byte-stable prefix and duplicates DateSource (which owns "today" now).
        assert!(
            !block.contains("Current date and time"),
            "runtime block must not carry a timestamp: {block}"
        );
    }

    #[test]
    fn memory_guidance_block_mentions_capture_and_recall_tools() {
        let block = memory_guidance_block();
        assert!(
            block.contains("core_memory_edit"),
            "guidance should tell the model to capture via core_memory_edit: {block}"
        );
        assert!(
            block.contains("memory_search"),
            "guidance should tell the model to recall via memory_search: {block}"
        );
        assert!(
            block.contains("persistent long-term memory"),
            "guidance should frame it as persistent memory: {block}"
        );
    }

    // ─── switch_project / rebind_project_dir (Issue #19) ───────────────────

    /// A path-bound source for testing: holds a dir in an interior-mutable
    /// cell, returns content derived from the current dir so rebind is
    /// observable via `load()`.
    struct PathBoundStub {
        dir: Arc<Mutex<std::path::PathBuf>>,
    }

    impl PathBoundStub {
        fn new(dir: std::path::PathBuf) -> Self {
            Self {
                dir: Arc::new(Mutex::new(dir)),
            }
        }
    }

    #[async_trait]
    impl ContextSource for PathBoundStub {
        fn key(&self) -> &str {
            "path_bound_stub"
        }
        async fn load(&self) -> Result<String> {
            let dir = self.dir.lock().unwrap().clone();
            Ok(format!("ctx-for:{}", dir.display()))
        }
        fn is_path_bound(&self) -> bool {
            true
        }
        fn rebind_project_dir(&self, dir: &Path) -> bool {
            *self.dir.lock().unwrap() = dir.to_path_buf();
            true
        }
    }

    #[test]
    fn has_path_bound_sources_reflects_source_set() {
        // Empty assembler → no path-bound sources.
        let ca = ContextAssembler::new();
        assert!(!ca.has_path_bound_sources());

        // Ambient-only (StubSource defaults is_path_bound=false) → still none.
        let ca = ContextAssembler::with_context_sources(vec![Arc::new(StubSource {
            key: "stub",
            content: "x",
        })]);
        assert!(!ca.has_path_bound_sources());

        // A path-bound source → true.
        let ca = ContextAssembler::with_context_sources(vec![Arc::new(PathBoundStub::new(
            std::path::PathBuf::from("/proj-a"),
        ))]);
        assert!(ca.has_path_bound_sources());
    }

    #[tokio::test]
    async fn rebind_project_dir_updates_sources_and_clears_cache() {
        let stub = Arc::new(PathBoundStub::new(std::path::PathBuf::from("/proj-a")));
        let mut ca =
            ContextAssembler::with_context_sources(vec![stub.clone() as Arc<dyn ContextSource>]);

        // Prime the cache with proj-a content.
        ca.refresh_sources().await.unwrap();
        let state = LoopState::new("task");
        let text_a = text_of(&ca.assemble(&state).unwrap());
        assert!(text_a.contains("ctx-for:/proj-a"));

        // Rebind to proj-b. The cache entry is dropped so the next assemble
        // can't serve stale proj-a content; refresh re-reads proj-b.
        let n = ca.rebind_project_dir(std::path::Path::new("/proj-b"));
        assert_eq!(n, 1);
        // Immediately after rebind (no refresh yet), the stale cache is gone.
        let state2 = LoopState::new("task2");
        let text_pre = text_of(&ca.assemble(&state2).unwrap());
        assert!(
            !text_pre.contains("ctx-for:/proj-a"),
            "stale proj-a content must not survive rebind: {text_pre}"
        );

        // After refresh, the new project's content is injected.
        ca.refresh_sources().await.unwrap();
        let state3 = LoopState::new("task3");
        let text_b = text_of(&ca.assemble(&state3).unwrap());
        assert!(text_b.contains("ctx-for:/proj-b"));
    }

    // ─── Stale tool-result truncation (per-iteration, ephemeral) ──────────────

    /// Helper: a Tool message with a single ToolResult block of `n` 'x' chars.
    fn tool_msg(n: usize) -> oneai_core::Message {
        oneai_core::Message::tool_result("call".to_string(), "x".repeat(n))
    }

    fn tool_content(m: &oneai_core::Message) -> &str {
        match &m.content[0] {
            oneai_core::ContentBlock::ToolResult { content, .. } => content,
            _ => unreachable!(),
        }
    }

    #[test]
    fn truncates_stale_tool_results_keeps_last_n_full() {
        // 6 tool results, each 5000 chars. The last 4 stay full; the first 2
        // are capped to ~2000 chars + a pointer.
        let mut conv = oneai_core::Conversation::with_id("c".into());
        for _ in 0..6 {
            conv.add_message(tool_msg(5000));
        }
        let ca = ContextAssembler::new();
        ca.truncate_stale_tool_results(&mut conv);
        let tools: Vec<&oneai_core::Message> = conv
            .messages
            .iter()
            .filter(|m| m.role == oneai_core::Role::Tool)
            .collect();
        assert_eq!(tools.len(), 6);
        // Last 4: full (5000 'x').
        for t in &tools[2..] {
            assert_eq!(
                tool_content(t).len(),
                5000,
                "recent tool result must stay full"
            );
        }
        // First 2 (stale): truncated.
        for t in &tools[0..2] {
            let c = tool_content(t);
            assert!(
                c.contains("[...truncated"),
                "stale result must be truncated"
            );
            assert!(c.len() < 5000, "stale result must be shorter than original");
            // Snippet is 2000 'x' + the pointer line; verify the head survived.
            assert!(c.starts_with(&"x".repeat(100)), "snippet head preserved");
        }
    }

    #[test]
    fn leaves_short_tool_results_untouched() {
        // 6 tool results, each 500 chars — all under the 2000 cap → no truncation.
        let mut conv = oneai_core::Conversation::with_id("c".into());
        for _ in 0..6 {
            conv.add_message(tool_msg(500));
        }
        let ca = ContextAssembler::new();
        ca.truncate_stale_tool_results(&mut conv);
        for m in &conv.messages {
            assert_eq!(tool_content(m).len(), 500, "short result must be untouched");
            assert!(!tool_content(m).contains("[...truncated"));
        }
    }

    #[test]
    fn no_truncation_when_fewer_than_keep_full() {
        // 3 tool results (≤ KEEP_FULL_RECENT=4) → nothing is stale → no truncation.
        let mut conv = oneai_core::Conversation::with_id("c".into());
        for _ in 0..3 {
            conv.add_message(tool_msg(5000));
        }
        let ca = ContextAssembler::new();
        ca.truncate_stale_tool_results(&mut conv);
        for m in &conv.messages {
            assert_eq!(tool_content(m).len(), 5000, "≤4 tool results → all full");
        }
    }

    // ─── Issue #40 context snapshot (build_context_snapshot) ──────────────

    use oneai_core::{ContextKey, Message};

    fn accounting(total: u32) -> oneai_core::ContextAccounting {
        let mut a = oneai_core::ContextAccounting::default();
        a.total_tokens = total;
        a.tool_call_tokens = 50;
        a
    }

    #[test]
    fn snapshot_classifies_all_section_kinds() {
        let mut conv = Conversation::with_id("c".into());
        conv.add_message(Message::system("You are OneAI."));
        conv.add_message(Message::user("first question"));
        conv.add_message(Message::assistant("an answer"));
        conv.add_message(Message::system("[Context: git_status] M src/main.rs"));
        conv.add_message(Message::system("[Context: core_memory] user likes rust"));
        conv.add_message(Message::system(
            "[Task Anchor] (do not compress)\n原始任务: first question",
        ));
        conv.add_message(Message::system(
            "[Plan & Progress] (do not compress)\n目标: x",
        ));
        conv.add_message(Message::system(
            "[Decisions Made] (do not compress)\n• a → b",
        ));
        conv.add_message(Message::system("[Blockers] (do not compress)\n⚠ b1: stuck"));
        conv.add_message(Message::system(
            "# Available skills\n- skill-creator: creates skills",
        ));
        conv.add_message(Message::system(
            "[Background tasks] (live)\n- `t1` (Code, Running): x",
        ));
        conv.add_message(Message::user("latest question"));

        let tools = vec![oneai_core::ToolDefinition {
            name: "shell".into(),
            description: "run a command".into(),
            parameters_schema: serde_json::json!({"type":"object"}),
        }];
        let mut hashes = std::collections::HashMap::new();
        let snap = build_context_snapshot(
            1,
            "test-model",
            &conv,
            &tools,
            &accounting(1000),
            &mut hashes,
        );

        let keys: Vec<&ContextKey> = snap.sections.iter().map(|s| &s.key).collect();
        assert!(keys.contains(&&ContextKey::BasePrompt));
        assert!(keys.contains(&&ContextKey::Context("git_status".into())));
        assert!(keys.contains(&&ContextKey::Context("core_memory".into())));
        assert!(keys.contains(&&ContextKey::TaskAnchor));
        assert!(keys.contains(&&ContextKey::PlanProgress));
        assert!(keys.contains(&&ContextKey::Decisions));
        assert!(keys.contains(&&ContextKey::Blockers));
        assert!(keys.contains(&&ContextKey::SkillMenu));
        assert!(keys.contains(&&ContextKey::BackgroundTasks));
        assert!(keys.contains(&&ContextKey::Tools));
        assert!(keys.contains(&&ContextKey::LatestUser));
        assert!(keys.contains(&&ContextKey::History));

        // First iteration: every section carries full content.
        assert!(snap.sections.iter().all(|s| s.content.is_some()));
        // Latest user message is the LAST user message, not the first.
        let latest = snap
            .sections
            .iter()
            .find(|s| s.key == ContextKey::LatestUser)
            .unwrap();
        assert_eq!(latest.content.as_deref(), Some("latest question"));
        // Base prompt is the un-prefixed system message.
        let base = snap
            .sections
            .iter()
            .find(|s| s.key == ContextKey::BasePrompt)
            .unwrap();
        assert_eq!(base.content.as_deref(), Some("You are OneAI."));
    }

    #[test]
    fn snapshot_dedups_unchanged_sections_within_turn() {
        let mut conv = Conversation::with_id("c".into());
        conv.add_message(Message::system("You are OneAI."));
        conv.add_message(Message::user("q"));

        let mut hashes = std::collections::HashMap::new();
        let snap1 = build_context_snapshot(1, "m", &conv, &[], &accounting(100), &mut hashes);
        // Second iteration, identical assembly → every section deduped.
        let snap2 = build_context_snapshot(2, "m", &conv, &[], &accounting(100), &mut hashes);
        assert!(snap1.sections.iter().all(|s| s.content.is_some()));
        assert!(
            snap2.sections.iter().all(|s| s.content.is_none()),
            "unchanged sections must omit content on later iterations"
        );

        // Mutate the user message → only LatestUser (+ History count) resend.
        conv.add_message(Message::assistant("a"));
        conv.add_message(Message::user("q2"));
        let snap3 = build_context_snapshot(3, "m", &conv, &[], &accounting(120), &mut hashes);
        let with_content: Vec<&ContextKey> = snap3
            .sections
            .iter()
            .filter(|s| s.content.is_some())
            .map(|s| &s.key)
            .collect();
        assert!(with_content.contains(&&ContextKey::LatestUser));
        assert!(with_content.contains(&&ContextKey::History));
        assert!(!with_content.contains(&&ContextKey::BasePrompt));
    }

    #[test]
    fn snapshot_tokens_use_accounting_for_tools() {
        let mut conv = Conversation::with_id("c".into());
        conv.add_message(Message::system("sys"));
        conv.add_message(Message::user("q"));
        let tools = vec![oneai_core::ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            parameters_schema: serde_json::json!({}),
        }];
        let mut hashes = std::collections::HashMap::new();
        let snap = build_context_snapshot(1, "m", &conv, &tools, &accounting(900), &mut hashes);
        let tools_sec = snap
            .sections
            .iter()
            .find(|s| s.key == ContextKey::Tools)
            .unwrap();
        assert_eq!(tools_sec.tokens, 50); // accounting.tool_call_tokens
        let history = snap
            .sections
            .iter()
            .find(|s| s.key == ContextKey::History)
            .unwrap();
        // Residual: total − named sections (saturating, never negative).
        let named: u64 = snap
            .sections
            .iter()
            .filter(|s| s.key != ContextKey::History)
            .map(|s| s.tokens)
            .sum();
        assert_eq!(history.tokens, (900u64).saturating_sub(named));
    }

    #[test]
    fn does_not_touch_assistant_or_user_messages() {
        // A large assistant Text block must survive even when stale tool results
        // around it are truncated — the model's own reasoning/output is never cut.
        let mut conv = oneai_core::Conversation::with_id("c".into());
        conv.add_message(oneai_core::Message::assistant("a".repeat(8000)));
        for _ in 0..6 {
            conv.add_message(tool_msg(5000));
        }
        conv.add_message(oneai_core::Message::user("b".repeat(8000)));
        let ca = ContextAssembler::new();
        ca.truncate_stale_tool_results(&mut conv);
        // Assistant + user Text blocks untouched (still 8000 chars each).
        let assistant_text = conv
            .messages
            .iter()
            .find(|m| m.role == oneai_core::Role::Assistant)
            .and_then(|m| match &m.content[0] {
                oneai_core::ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            assistant_text.len(),
            8000,
            "assistant text must not be truncated"
        );
        let user_text = conv
            .messages
            .iter()
            .find(|m| m.role == oneai_core::Role::User)
            .and_then(|m| match &m.content[0] {
                oneai_core::ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .unwrap();
        assert_eq!(user_text.len(), 8000, "user text must not be truncated");
    }
}
