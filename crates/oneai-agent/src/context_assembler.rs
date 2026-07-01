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
    /// Cached context content from sources (for OnChange detection).
    cached_context: HashMap<String, String>,
    /// Baseline context content (from the first epoch — for diffing in incremental mode).
    baseline_content: HashMap<String, String>,
    /// Whether initial load has been done (for OnceAtStart sources).
    initial_load_done: bool,
    /// Whether the baseline epoch has been injected by `assemble()`.
    /// Replaces the old `last_snapshot.is_none()` first-epoch signal — the
    /// assembler now self-manages its epoch state instead of relying on the
    /// loop to feed it an external snapshot.
    baseline_injected: bool,
    /// Number of iterations since the first epoch (for Periodic sources in incremental mode).
    iterations_since_epoch: Option<usize>,
}

impl ContextAssembler {
    /// Create a new context assembler.
    pub fn new() -> Self {
        Self {
            context_sources: Vec::new(),
            cached_context: HashMap::new(),
            baseline_content: HashMap::new(),
            initial_load_done: false,
            baseline_injected: false,
            iterations_since_epoch: None,
        }
    }

    /// Create a context assembler with domain-specific context sources.
    pub fn with_context_sources(context_sources: Vec<Arc<dyn ContextSource>>) -> Self {
        Self {
            context_sources,
            cached_context: HashMap::new(),
            baseline_content: HashMap::new(),
            initial_load_done: false,
            baseline_injected: false,
            iterations_since_epoch: None,
        }
    }

    /// Assemble the context for a loop iteration.
    ///
    /// **Context Epoch mode**:
    /// - First epoch (`baseline_injected` is false): inject every context
    ///   source's cached content once, in priority order. This establishes the
    ///   "baseline epoch" the model remembers; subsequent iterations only need
    ///   the diff to stay current.
    /// - Subsequent epochs: inject only what each source's `RefreshPolicy`
    ///   permits (OnChange → only if changed since baseline; EveryIteration →
    ///   always; Periodic → on its interval; OnceAtStart → never again).
    ///   This is where the token savings happen — instead of re-injecting the
    ///   entire file tree, git status, and all sources every turn, we inject
    ///   only the changes.
    pub fn assemble(&mut self, state: &crate::agent_loop::LoopState) -> Result<Conversation> {
        let mut conversation = state.conversation.clone();

        let is_first_epoch = !self.baseline_injected;

        if is_first_epoch {
            // First epoch: inject the full baseline — every source's cached
            // content, in priority order. Refresh policies only gate
            // *re*-injection on subsequent epochs; on the baseline epoch every
            // source with cached content is injected once. (refresh_sources()
            // populated the cache just before this call.)
            self.inject_sources(&mut conversation, |policy, _key| {
                // Baseline: inject everything regardless of policy.
                let _ = policy;
                true
            });
            self.baseline_injected = true;
        } else {
            // Incremental epoch: inject only the diff from the baseline.
            // This is where the token savings happen — instead of re-injecting
            // the entire file tree, git status, and all context sources every
            // turn, we only inject the changes.
            self.inject_sources(&mut conversation, |policy, key| {
                should_reinject(policy, key, self)
            });
        }

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

    /// Check if a context source's content has changed since baseline.
    fn has_source_changed(&self, key: &str) -> bool {
        // In incremental mode, sources that changed since baseline should be re-injected.
        // For now, we check if the cached content differs from the baseline content.
        // The baseline_content map stores what was loaded during the first epoch.
        if let Some(baseline) = self.baseline_content.get(key) {
            if let Some(current) = self.cached_context.get(key) {
                baseline != current
            } else {
                false
            }
        } else {
            // Not in baseline — this is a new source, inject it
            true
        }
    }

    /// Refresh and cache all context sources (async — called from the loop).
    ///
    /// On the first call (baseline epoch), stores all source content as baseline.
    /// On subsequent calls, only updates cached content for changed sources.
    pub async fn refresh_sources(&mut self) -> Result<()> {
        if !self.initial_load_done {
            // First epoch: store all content as baseline for later diffing
            for source in &self.context_sources {
                let content = source.load().await?;
                self.cached_context.insert(source.key().to_string(), content.clone());
                // Store baseline content (never changes after first epoch)
                self.baseline_content.insert(source.key().to_string(), content);
            }
            self.initial_load_done = true;
            self.iterations_since_epoch = Some(0);
        } else {
            // Incremental epoch: update cache, only for changed sources
            for source in &self.context_sources {
                let content = source.load().await?;
                let prev = self.cached_context.get(source.key());
                if prev.is_none_or(|p| p != &content) {
                    self.cached_context.insert(source.key().to_string(), content);
                }
            }
            // Increment the epoch counter
            if let Some(ref count) = self.iterations_since_epoch {
                self.iterations_since_epoch = Some(count + 1);
            }
        }

        Ok(())
    }
}

impl Default for ContextAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Decide whether a source should be re-injected on an incremental epoch,
/// given its `RefreshPolicy`.
///
/// Free-standing so it can be unit-tested independently of `assemble()`.
fn should_reinject(
    policy: &oneai_domain::context_source::RefreshPolicy,
    key: &str,
    assembler: &ContextAssembler,
) -> bool {
    use oneai_domain::context_source::RefreshPolicy;
    match policy {
        // EveryIteration: always inject (this source changes every turn)
        RefreshPolicy::EveryIteration => true,
        // OnceAtStart: skip (already in baseline, no need to repeat)
        RefreshPolicy::OnceAtStart => false,
        // OnChange: inject only if content changed from baseline
        RefreshPolicy::OnChange => {
            assembler.cached_context.contains_key(key) && assembler.has_source_changed(key)
        }
        // Periodic: check if enough iterations passed since baseline
        // Convert Duration to an approximate iteration count
        // (assume ~5 seconds per iteration as rough estimate)
        RefreshPolicy::Periodic(interval) => {
            let interval_iters = (interval.as_secs() / 5).max(1) as usize;
            if let Some(iterations) = assembler.iterations_since_epoch {
                iterations % interval_iters == 0 && iterations > 0
            } else {
                false
            }
        }
        _ => true, // #[non_exhaustive] catch-all
    }
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

    /// Regression test for the Context Epoch first-epoch behaviour: the full
    /// baseline (every context source, including OnceAtStart ones) MUST be
    /// injected on the first assemble() call, and OnceAtStart sources MUST NOT
    /// be re-injected on subsequent (incremental) epochs.
    #[tokio::test]
    async fn first_epoch_injects_baseline_then_incremental_skips_once_at_start() {
        let sources: Vec<Arc<dyn ContextSource>> = vec![
            Arc::new(StubSource { key: "stub", content: "STUB-BASELINE-CONTENT" }),
        ];
        let mut ca = ContextAssembler::with_context_sources(sources);

        // First epoch: refresh stores baseline, assemble injects the source.
        ca.refresh_sources().await.unwrap();
        let state = LoopState::new("do something");
        let conv = ca.assemble(&state).unwrap();
        let text = text_of(&conv);
        assert!(text.contains("[Context: stub]"), "context source missing on first epoch: {text}");
        assert!(text.contains("STUB-BASELINE-CONTENT"), "baseline content missing: {text}");

        // Second epoch: incremental. OnceAtStart should NOT be re-injected.
        ca.refresh_sources().await.unwrap();
        let state2 = LoopState::new("next turn");
        let conv2 = ca.assemble(&state2).unwrap();
        let text2 = text_of(&conv2);
        assert!(!text2.contains("STUB-BASELINE-CONTENT"),
            "OnceAtStart source re-injected on incremental epoch: {text2}");
    }

    /// An OnChange source that changes between epochs is re-injected; one that
    /// doesn't change is not. This is the path that replaces the old
    /// hardcoded environment-snapshot diff.
    #[tokio::test]
    async fn on_change_source_reinjects_only_when_content_changes() {
        let content = Arc::new(Mutex::new("STUB-A".to_string()));
        let sources: Vec<Arc<dyn ContextSource>> = vec![
            Arc::new(MutableStubSource { key: "stub", content: content.clone() }),
        ];
        let mut ca = ContextAssembler::with_context_sources(sources);

        // First epoch: baseline A is injected.
        ca.refresh_sources().await.unwrap();
        let conv = ca.assemble(&LoopState::new("t1")).unwrap();
        assert!(text_of(&conv).contains("STUB-A"), "baseline missing: {}", text_of(&conv));

        // Second epoch, no change: OnChange source is NOT re-injected.
        ca.refresh_sources().await.unwrap();
        let conv2 = ca.assemble(&LoopState::new("t2")).unwrap();
        assert!(!text_of(&conv2).contains("STUB-A"),
            "unchanged OnChange source re-injected: {}", text_of(&conv2));

        // Third epoch, content changes: OnChange source IS re-injected with new content.
        *content.lock().unwrap() = "STUB-B".to_string();
        ca.refresh_sources().await.unwrap();
        let conv3 = ca.assemble(&LoopState::new("t3")).unwrap();
        assert!(text_of(&conv3).contains("STUB-B"),
            "changed content not re-injected: {}", text_of(&conv3));
    }

    #[test]
    fn runtime_context_block_has_date_and_search_guidance() {
        let block = runtime_context_block();
        assert!(block.contains("Current date and time"), "block: {block}");
        assert!(block.contains("web_search"), "block should nudge web_search: {block}");
    }
}
