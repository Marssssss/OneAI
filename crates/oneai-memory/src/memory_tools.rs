//! Self-managed memory tools (Letta-style) — let the agent curate its own memory.
//!
//! These are the "越用越好用" engine. When a `MemoryProfile` opts in
//! (`enable_memory_tools`), the `AppBuilder` registers these tools so the
//! model can actively manage its memory across turns and sessions:
//!
//! - `memory_search`: recall facts from the archival tier (semantic/keyword).
//! - `core_memory_edit`: upsert a fact into the always-in-context core tier
//!   (conflict-resolved — same `subject+predicate` updates rather than
//!   duplicating, so the agent can revise beliefs as it learns).
//! - `archival_memory_insert`: explicitly archive a fact for later recall.
//!
//! All three namespace facts by the `MemoryManager`'s current `user_id` /
//! `session_id`, so habits persist across sessions (user scope) while
//! episodic context stays session-scoped.

use std::sync::Arc;

use oneai_core::error::Result;
use oneai_core::{FactType, MemoryFact, RiskLevel, Tool, ToolOutput};
use chrono::Utc;

use crate::manager::MemoryManager;

/// Helper: build a `MemoryFact` from tool args, namespaced by the manager.
async fn build_fact(mm: &MemoryManager, fact_type: String, subject: String, predicate: String, content: String, importance: f32) -> MemoryFact {
    MemoryFact {
        id: format!("fact_{}", uuid::Uuid::new_v4()),
        user_id: mm.user_id().await,
        session_id: mm.session_id().await,
        fact_type: FactType::new(fact_type),
        subject,
        predicate,
        content,
        embedding: None,
        metadata: std::collections::HashMap::new(),
        importance: importance.clamp(0.0, 1.0),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        version: 1,
    }
}

/// Read the optional `importance` field (0.0–1.0) from tool args, defaulting to
/// the per-type baseline so the agent can override salience when it matters.
fn read_importance(args: &serde_json::Value, fact_type: &str) -> f32 {
    args.get("importance")
        .and_then(|v| v.as_f64())
        .filter(|v| (0.0..=1.0).contains(v))
        .map(|v| v as f32)
        .unwrap_or_else(|| default_tool_importance(fact_type))
}

/// Per-type default importance for agent-curated facts (mirrors the
/// FactExtractor's `default_importance_for_type`).
fn default_tool_importance(fact_type: &str) -> f32 {
    match fact_type {
        "decision" | "episodic" => 0.85,
        "critical_file" => 0.75,
        "open_task" | "user_tooling_pref" | "user_interest" => 0.65,
        _ => 0.5,
    }
}

/// `memory_search` — recall facts from the archival tier.
pub struct MemorySearchTool {
    mm: Arc<MemoryManager>,
}

impl MemorySearchTool {
    pub fn new(mm: Arc<MemoryManager>) -> Self {
        Self { mm }
    }
}

#[async_trait::async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str { "memory_search" }

    fn description(&self) -> &str {
        "Search your long-term/archival memory for facts relevant to a query. \
        Use this to recall past decisions, user preferences, or context from \
        earlier in this session or previous sessions. Returns matching facts."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to recall (keywords or a natural-language query)" },
                "top_k": { "type": "integer", "description": "Max facts to return (default 5)", "default": 5 }
            },
            "required": ["query"]
        })
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::Low }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let top_k = args.get("top_k").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
        if query.is_empty() {
            return Ok(ToolOutput { success: false, content: String::new(), error: Some("query is required".into()) });
        }

        // Canonical path: three-factor search over the archival fact tier.
        let facts = self.mm.fact_archive().search_hybrid(None, query, top_k, true).await;

        if !facts.is_empty() {
            let content = facts.iter()
                .map(|f| format!("- [{}] {} {}: {}", f.fact_type, f.subject, f.predicate, f.content))
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(ToolOutput { success: true, content, error: None });
        }

        // Fallback (R2): raw-transcript回溯. When no fact matches, search the
        // persisted conversation snapshot for the current session — this is the
        // on-demand ground-truth path for纠错核实 / 条件分支回溯 / 审计. Only
        // reachable when facts are insufficient (常态不自动召回原文).
        let snapshot_hits = self.search_conversation_snapshot(query, top_k).await;
        let content = if snapshot_hits.is_empty() {
            "No matching memories found.".to_string()
        } else {
            let mut out = String::from("[Raw transcript recall — no structured fact matched]\n");
            out.push_str(&snapshot_hits.join("\n"));
            out
        };
        Ok(ToolOutput { success: true, content, error: None })
    }
}

impl MemorySearchTool {
    /// Keyword-filter the current session's persisted conversation snapshot.
    ///
    /// Returns matching message excerpts (role + text). Empty when there is no
    /// persistence backend or no saved conversation for this session.
    async fn search_conversation_snapshot(&self, query: &str, top_k: usize) -> Vec<String> {
        let Some(p) = self.mm.persistence() else { return Vec::new() };
        let session_id = self.mm.session_id().await;
        if session_id.is_empty() { return Vec::new(); }
        let conv = match p.load_conversation(&session_id).await {
            Ok(Some(c)) => c,
            _ => return Vec::new(),
        };
        conv.messages.iter()
            .filter_map(|m| {
                let text = m.text_content();
                if oneai_core::keyword_matches(&text, query) {
                    let role = match m.role {
                        oneai_core::Role::User => "user",
                        oneai_core::Role::Assistant => "assistant",
                        oneai_core::Role::Tool => "tool",
                        _ => "system",
                    };
                    // Cap each excerpt so a single huge message can't dominate.
                    let body: String = text.chars().take(1000).collect();
                    Some(format!("- [{}] {}", role, body))
                } else {
                    None
                }
            })
            .take(top_k)
            .collect()
    }
}

/// `core_memory_edit` — upsert a fact into the always-in-context core tier.
///
/// Conflict-resolved: if a fact with the same `subject+predicate` already
/// exists in core, it is updated (version bumped) rather than duplicated —
/// letting the agent revise its beliefs as it learns more.
pub struct CoreMemoryEditTool {
    mm: Arc<MemoryManager>,
}

impl CoreMemoryEditTool {
    pub fn new(mm: Arc<MemoryManager>) -> Self {
        Self { mm }
    }
}

#[async_trait::async_trait]
impl Tool for CoreMemoryEditTool {
    fn name(&self) -> &str { "core_memory_edit" }

    fn description(&self) -> &str {
        "Add or update a fact in your always-on core memory (the facts you see \
        every turn). If a fact with the same subject+predicate already exists, \
        it is updated with the new value. Use this to record durable user \
        preferences, key decisions, and current task state. IMPORTANT \
        (constraint sedimentation): persistent constraints — package manager \
        to use, modules never to touch, token/step budgets, coding standards \
        — should be written here so they stay salient every turn and do NOT \
        depend on being recalled from history (long context degrades \
        attention to early constraints)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "fact_type": { "type": "string", "description": "Category (e.g. user_tooling_pref, decision, open_task)" },
                "subject": { "type": "string", "description": "What the fact is about, e.g. 'user.package_manager'" },
                "predicate": { "type": "string", "description": "The assertion, e.g. 'prefers', 'decided_to', 'status_is'" },
                "content": { "type": "string", "description": "The fact's value, e.g. 'pnpm'" },
                "importance": { "type": "number", "description": "Optional salience 0.0–1.0 for recall ranking; omit to use the per-type default" }
            },
            "required": ["fact_type", "subject", "predicate", "content"]
        })
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::Medium }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let get = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let fact_type = get("fact_type");
        let subject = get("subject");
        let predicate = get("predicate");
        let content = get("content");
        if subject.is_empty() || predicate.is_empty() || content.is_empty() {
            return Ok(ToolOutput { success: false, content: String::new(), error: Some("subject, predicate, and content are required".into()) });
        }
        let importance = read_importance(&args, &fact_type);
        let fact = build_fact(&self.mm, fact_type, subject, predicate, content, importance).await;
        let outcome = self.mm.core_memory().upsert(fact).await;
        // Enforce the core budget — evicted facts go to archival (paging).
        let evicted = self.mm.core_memory().enforce_budget().await;
        if !evicted.is_empty() {
            self.mm.archive_facts(evicted).await;
        }
        let msg = match outcome {
            crate::fact_store::UpsertOutcome::Inserted => format!("Inserted core fact: {} {}.", args.get("subject").and_then(|v| v.as_str()).unwrap_or(""), args.get("predicate").and_then(|v| v.as_str()).unwrap_or("")),
            crate::fact_store::UpsertOutcome::Updated { previous_version } => format!("Updated core fact (v{}→v{}).", previous_version, previous_version + 1),
        };
        Ok(ToolOutput { success: true, content: msg, error: None })
    }
}

/// `archival_memory_insert` — explicitly archive a fact for later recall.
pub struct ArchivalInsertTool {
    mm: Arc<MemoryManager>,
}

impl ArchivalInsertTool {
    pub fn new(mm: Arc<MemoryManager>) -> Self {
        Self { mm }
    }
}

#[async_trait::async_trait]
impl Tool for ArchivalInsertTool {
    fn name(&self) -> &str { "archival_memory_insert" }

    fn description(&self) -> &str {
        "Store a fact in archival memory for later recall via memory_search. \
        Use this for facts worth keeping but not needed every turn (e.g. a \
        resolved decision, a reference, a one-off observation)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "fact_type": { "type": "string", "description": "Category (e.g. decision, source, claim)" },
                "subject": { "type": "string", "description": "What the fact is about" },
                "predicate": { "type": "string", "description": "The assertion" },
                "content": { "type": "string", "description": "The fact's value" },
                "importance": { "type": "number", "description": "Optional salience 0.0–1.0 for recall ranking; omit to use the per-type default" }
            },
            "required": ["fact_type", "subject", "predicate", "content"]
        })
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::Medium }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let get = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let fact_type = get("fact_type");
        let subject = get("subject");
        let predicate = get("predicate");
        let content = get("content");
        if subject.is_empty() || predicate.is_empty() || content.is_empty() {
            return Ok(ToolOutput { success: false, content: String::new(), error: Some("subject, predicate, and content are required".into()) });
        }
        let importance = read_importance(&args, &fact_type);
        let fact = build_fact(&self.mm, fact_type, subject, predicate, content, importance).await;
        self.mm.archive_facts(vec![fact]).await;
        Ok(ToolOutput { success: true, content: "Fact archived.".to_string(), error: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mm() -> Arc<MemoryManager> {
        Arc::new(MemoryManager::new())
    }

    async fn mm_alice() -> Arc<MemoryManager> {
        let mm = mm();
        mm.set_user_id("alice").await;
        mm
    }

    fn args(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[tokio::test]
    async fn core_memory_edit_inserts_then_updates() {
        let mm = mm_alice().await;
        mm.set_session_id("s1").await;
        let tool = CoreMemoryEditTool::new(mm.clone());
        let r = tool.execute(args(r#"{"fact_type":"user_tooling_pref","subject":"user.pm","predicate":"prefers","content":"npm"}"#)).await.unwrap();
        assert!(r.success);
        assert_eq!(mm.core_memory().facts().await.len(), 1);

        // Same key → update, not duplicate.
        let r = tool.execute(args(r#"{"fact_type":"user_tooling_pref","subject":"user.pm","predicate":"prefers","content":"pnpm"}"#)).await.unwrap();
        assert!(r.success);
        assert!(r.content.contains("Updated"));
        assert_eq!(mm.core_memory().facts().await.len(), 1);
        assert_eq!(mm.core_memory().facts().await[0].content, "pnpm");
        assert_eq!(mm.core_memory().facts().await[0].version, 2);
        assert_eq!(mm.core_memory().facts().await[0].user_id, "alice");
    }

    #[tokio::test]
    async fn archival_insert_and_search_roundtrip() {
        let mm = mm();
        mm.set_session_id("s1").await;
        let insert = ArchivalInsertTool::new(mm.clone());
        let r = insert.execute(args(r#"{"fact_type":"decision","subject":"auth","predicate":"decided_to","content":"JWT"}"#)).await.unwrap();
        assert!(r.success);
        assert_eq!(mm.fact_archive().len().await, 1);

        let search = MemorySearchTool::new(mm.clone());
        let r = search.execute(args(r#"{"query":"auth"}"#)).await.unwrap();
        assert!(r.success);
        assert!(r.content.contains("JWT"));
    }

    #[tokio::test]
    async fn search_returns_none_when_empty() {
        let mm = mm();
        mm.set_session_id("s1").await;
        let search = MemorySearchTool::new(mm);
        let r = search.execute(args(r#"{"query":"anything"}"#)).await.unwrap();
        assert!(r.success);
        assert!(r.content.contains("No matching"));
    }
}
