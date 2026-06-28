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
async fn build_fact(mm: &MemoryManager, fact_type: String, subject: String, predicate: String, content: String) -> MemoryFact {
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
        created_at: Utc::now(),
        updated_at: Utc::now(),
        version: 1,
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
        let facts = self.mm.fact_archive().search_keyword(query, top_k).await;
        let content = if facts.is_empty() {
            "No matching memories found.".to_string()
        } else {
            facts.iter()
                .map(|f| format!("- [{}] {} {}: {}", f.fact_type, f.subject, f.predicate, f.content))
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ToolOutput { success: true, content, error: None })
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
        preferences, key decisions, and current task state."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "fact_type": { "type": "string", "description": "Category (e.g. user_tooling_pref, decision, open_task)" },
                "subject": { "type": "string", "description": "What the fact is about, e.g. 'user.package_manager'" },
                "predicate": { "type": "string", "description": "The assertion, e.g. 'prefers', 'decided_to', 'status_is'" },
                "content": { "type": "string", "description": "The fact's value, e.g. 'pnpm'" }
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
        let fact = build_fact(&self.mm, fact_type, subject, predicate, content).await;
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
                "content": { "type": "string", "description": "The fact's value" }
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
        let fact = build_fact(&self.mm, fact_type, subject, predicate, content).await;
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
