//! Context snapshot — a sectioned view of the fully-assembled inference
//! context for one iteration (issue #40 trajectory panel).
//!
//! Each iteration the [`crate`] agent loop assembles the request context from
//! many parts — base system prompt, context-source blocks (env sensing,
//! memory recall), pinned blocks (task anchor / plan / decisions / blockers /
//! skill menu), tool definitions, and the latest user question. The snapshot
//! breaks that assembly into labeled [`ContextSection`]s so a trajectory UI
//! can show *what the model actually saw* at each iteration.
//!
//! Wire-size discipline: within one turn the assembly is near-identical
//! across iterations, so a section whose [`ContextSection::content_hash`]
//! matches the previous emission carries `content: None` — the consumer keeps
//! the last content per key. The first iteration of a turn always sends full
//! content for every section.

use serde::{Deserialize, Serialize};

// ─── ContextKey ─────────────────────────────────────────────────────────────

/// Which part of the assembled context a section represents.
///
/// `Context(String)` carries the context-source key (env sensing,
/// `core_memory` recall, …); every other variant is a fixed well-known part.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContextKey {
    /// The base system prompt (first un-prefixed system message; runtime /
    /// memory-guidance / core-memory blocks are appended into it at session
    /// start).
    BasePrompt,
    /// A `[Context: <key>]` context-source block (env sensing, memory
    /// recall, …). The payload is the source key.
    Context(String),
    /// The pinned `[Task Anchor]` block (the original user task — the
    /// "fixed first question").
    TaskAnchor,
    /// The pinned `[Plan & Progress]` block.
    PlanProgress,
    /// The pinned `[Decisions Made]` block.
    Decisions,
    /// The pinned `[Blockers]` block.
    Blockers,
    /// The Tier-1 "Available skills" menu (skill names + descriptions).
    SkillMenu,
    /// The active skill's full instructions (progressive disclosure).
    ActiveSkill,
    /// The one-shot "# Newly available tools" note (self-extension).
    NewTools,
    /// The live `[Background tasks]` status block.
    BackgroundTasks,
    /// The tool definitions (schemas) sent with the request.
    Tools,
    /// The latest user message (the current question).
    LatestUser,
    /// The remaining conversation history, summarized as a message count
    /// (never sent verbatim — it's the transcript itself).
    History,
}

impl ContextKey {
    /// A stable short string usable as a dedup/cache key on the wire consumer.
    pub fn cache_key(&self) -> String {
        match self {
            ContextKey::BasePrompt => "base_prompt".to_string(),
            ContextKey::Context(k) => format!("context:{k}"),
            ContextKey::TaskAnchor => "task_anchor".to_string(),
            ContextKey::PlanProgress => "plan_progress".to_string(),
            ContextKey::Decisions => "decisions".to_string(),
            ContextKey::Blockers => "blockers".to_string(),
            ContextKey::SkillMenu => "skill_menu".to_string(),
            ContextKey::ActiveSkill => "active_skill".to_string(),
            ContextKey::NewTools => "new_tools".to_string(),
            ContextKey::BackgroundTasks => "background_tasks".to_string(),
            ContextKey::Tools => "tools".to_string(),
            ContextKey::LatestUser => "latest_user".to_string(),
            ContextKey::History => "history".to_string(),
        }
    }

    /// Human-facing label (English; frontends localize off the key).
    pub fn label(&self) -> &'static str {
        match self {
            ContextKey::BasePrompt => "system prompt",
            ContextKey::Context(_) => "context source",
            ContextKey::TaskAnchor => "task anchor",
            ContextKey::PlanProgress => "plan & progress",
            ContextKey::Decisions => "decisions",
            ContextKey::Blockers => "blockers",
            ContextKey::SkillMenu => "skill menu",
            ContextKey::ActiveSkill => "active skill",
            ContextKey::NewTools => "new tools",
            ContextKey::BackgroundTasks => "background tasks",
            ContextKey::Tools => "tool definitions",
            ContextKey::LatestUser => "latest user message",
            ContextKey::History => "history",
        }
    }
}

// ─── ContextSection / ContextSnapshot ───────────────────────────────────────

/// One labeled part of the assembled context.
///
/// `content` is `None` when [`content_hash`](Self::content_hash) equals the
/// previous emission's hash for the same key within the turn — the consumer
/// keeps its cached content. `History` never carries content (count only,
/// rendered into `content` as a summary line by the producer is allowed but
/// consumers must treat it as informational).
// NOTE: deliberately NOT `#[non_exhaustive]` — constructed cross-crate by the
// agent loop's snapshot builder (struct expressions can't cross the
// non-exhaustive boundary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSection {
    /// Which part of the assembly this is.
    pub key: ContextKey,
    /// Human-facing label (see [`ContextKey::label`]).
    pub label: String,
    /// Estimated tokens this section occupies.
    pub tokens: u64,
    /// FNV-stable content hash — dedup key across iterations within a turn.
    pub content_hash: u64,
    /// The section content; `None` when unchanged since the previous emission.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// The full sectioned context snapshot for one iteration.
// NOTE: deliberately NOT `#[non_exhaustive]` — see `ContextSection`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    /// The iteration this assembly belongs to.
    pub iteration: usize,
    /// The sections, in assembly order.
    pub sections: Vec<ContextSection>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_key_serde_roundtrip() {
        let keys = [
            ContextKey::BasePrompt,
            ContextKey::Context("core_memory".to_string()),
            ContextKey::Tools,
            ContextKey::LatestUser,
        ];
        for key in &keys {
            let json = serde_json::to_string(key).unwrap();
            let back: ContextKey = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, key);
        }
        // Tagged wire form.
        assert_eq!(
            serde_json::to_string(&ContextKey::BasePrompt).unwrap(),
            r#"{"type":"base_prompt"}"#
        );
        assert_eq!(
            serde_json::to_string(&ContextKey::Context("git".to_string())).unwrap(),
            r#"{"type":"context","value":"git"}"#
        );
    }

    #[test]
    fn section_skips_none_content() {
        let section = ContextSection {
            key: ContextKey::Tools,
            label: "tool definitions".to_string(),
            tokens: 100,
            content_hash: 42,
            content: None,
        };
        let json = serde_json::to_string(&section).unwrap();
        // `content_hash` stays; the `content` key itself is omitted.
        assert!(!json.contains(r#""content":""#), "{json}");

        let with_content = ContextSection {
            content: Some("[…]".to_string()),
            ..section
        };
        let json2 = serde_json::to_string(&with_content).unwrap();
        assert!(json2.contains(r#""content":""#));
    }

    #[test]
    fn cache_keys_are_distinct() {
        let a = ContextKey::Context("core_memory".to_string()).cache_key();
        let b = ContextKey::Context("git_status".to_string()).cache_key();
        assert_ne!(a, b);
        assert_eq!(a, "context:core_memory");
    }
}
