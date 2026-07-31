//! `skill_manage` tool — model-driven skill lifecycle control (Phase 2.1
//! Stage B).
//!
//! Where the `skill` tool *activates* a skill (and bumps its use-count), this
//! tool lets the model — primarily the cadence-fired `Reflect` sub-agent —
//! *curate* the skill library: archive a skill that has stopped earning its
//! schema footprint, restore one that's relevant again, pin a skill so the
//! curator never auto-retires it, or list the current lifecycle state. This
//! closes the Hermes-style loop: the reflect sub-agent distills a durable
//! learning, then either persists it to memory (Stage A) or patches the skill
//! library (Stage B) via this tool.
//!
//! The tool is a thin wrapper over [`SkillCurator`]; it never touches the
//! filesystem directly (the curator + store own durability, including writing a
//! backup snapshot before any manual archive so the action is reversible).

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::Tool;
use oneai_core::{RiskLevel, ToolOutput};
use oneai_skill::curator::SkillCurator;
use serde::Deserialize;

/// A tool that lets the model curate the skill library.
pub struct SkillManageTool {
    curator: Arc<SkillCurator>,
}

impl SkillManageTool {
    /// Create a `skill_manage` tool backed by the given curator.
    pub fn new(curator: Arc<SkillCurator>) -> Self {
        Self { curator }
    }
}

/// The parsed action argument.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Action {
    /// Retire a skill (hide from the menu, keep on disk + in backups).
    Archive,
    /// Restore an archived skill to Active.
    Restore,
    /// Pin a skill (exempt from automatic retirement).
    Pin,
    /// Unpin a skill.
    Unpin,
    /// List each skill's lifecycle state / use_count / pinned / author.
    #[default]
    List,
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Curate the skill library: archive a skill that has stopped being useful, restore an \
        archived skill, pin a skill so it is never auto-retired, or list every skill's lifecycle \
        state. Use this — NOT the `skill` tool — to retire/restore/pin. Archiving writes a \
        restorable backup first; nothing is ever deleted."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["archive", "restore", "pin", "unpin", "list"],
                    "description": "The lifecycle action to perform. Defaults to `list`."
                },
                "skill": {
                    "type": "string",
                    "description": "The skill name (required for archive/restore/pin/unpin; ignored for list)."
                }
            },
            "required": ["action"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        // Curatorial actions are reversible (backup-before-archive, restore),
        // and the curator never deletes a skill. Low risk — but it does change
        // what the model sees next turn, so it stays Standard (not auto-approve
        // Read) so a permission profile can gate it if desired.
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let action =
            serde_json::from_value::<Action>(args.get("action").cloned().unwrap_or_default())
                .unwrap_or_default();
        let skill = args
            .get("skill")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match action {
            Action::List => {
                let rows = self.curator.status().await;
                let mut out = String::from("skill | state | use_count | pinned | author\n");
                for (s, m) in rows {
                    out.push_str(&format!(
                        "{} | {:?} | {} | {} | {:?}\n",
                        s.name, m.state, m.use_count, m.pinned, m.created_by
                    ));
                }
                Ok(ToolOutput {
                    success: true,
                    content: out,
                    error: None,
                    ..Default::default()
                })
            }
            Action::Archive | Action::Restore | Action::Pin | Action::Unpin => {
                if skill.is_empty() {
                    return Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!(
                            "action `{:?}` requires a `skill` name",
                            action_name(&action)
                        )),
                        ..Default::default()
                    });
                }
                // Guard: the named skill must be registered.
                if self.curator.registry().find_by_name(&skill).await.is_none() {
                    let available = self.curator.registry().skill_names().await.join(", ");
                    return Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!(
                            "Skill '{}' not registered. Available: {}",
                            skill, available
                        )),
                        ..Default::default()
                    });
                }
                let m = match action {
                    Action::Archive => self.curator.archive(&skill).await,
                    Action::Restore => self.curator.restore(&skill).await,
                    Action::Pin => self.curator.pin(&skill).await,
                    Action::Unpin => self.curator.unpin(&skill).await,
                    _ => unreachable!(),
                };
                Ok(ToolOutput {
                    success: true,
                    content: format!(
                        "skill `{}` now {:?} (use_count={}, pinned={}, created_by={:?})",
                        skill, m.state, m.use_count, m.pinned, m.created_by
                    ),
                    error: None,
                    ..Default::default()
                })
            }
        }
    }
}

/// Human-readable name for an `Action` variant (for error messages).
fn action_name(a: &Action) -> &'static str {
    match a {
        Action::Archive => "archive",
        Action::Restore => "restore",
        Action::Pin => "pin",
        Action::Unpin => "unpin",
        Action::List => "list",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::SkillDescriptor;
    use oneai_skill::curator::SkillCurator;
    use oneai_skill::lifecycle::{SkillLifecycleConfig, SkillMetadataStore, SkillState};
    use oneai_skill::SkillRegistry;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn make_skill(name: &str) -> SkillDescriptor {
        SkillDescriptor {
            name: name.into(),
            description: format!("desc {name}"),
            prompt_template: "body".into(),
            trigger_keywords: vec!["k".into()],
            ..Default::default()
        }
    }

    fn tmp_root() -> PathBuf {
        let name = std::thread::current().name().unwrap_or("test").to_string();
        let mut h: u64 = 1469598103934665603;
        for b in name.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let p = std::env::temp_dir().join(format!("oneai-skill-mgr-{h:x}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    async fn setup() -> (SkillManageTool, Arc<SkillMetadataStore>) {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(make_skill("foo")).await.unwrap();
        let store = Arc::new(SkillMetadataStore::new(
            tmp_root(),
            SkillLifecycleConfig::default(),
        ));
        store.load().await;
        let curator = Arc::new(SkillCurator::new(
            registry,
            Arc::clone(&store),
            HashSet::new(),
        ));
        (SkillManageTool::new(curator), store)
    }

    #[tokio::test]
    async fn list_returns_all_skills() {
        let (tool, _) = setup().await;
        let out = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(out.success);
        assert!(out.content.contains("foo"));
        assert!(out.content.contains("state"));
    }

    #[tokio::test]
    async fn archive_then_restore_round_trip() {
        let (tool, store) = setup().await;
        let out = tool
            .execute(serde_json::json!({"action": "archive", "skill": "foo"}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(store.get("foo").await.unwrap().state, SkillState::Archived);

        let out = tool
            .execute(serde_json::json!({"action": "restore", "skill": "foo"}))
            .await
            .unwrap();
        assert!(out.success);
        assert_eq!(store.get("foo").await.unwrap().state, SkillState::Active);
    }

    #[tokio::test]
    async fn pin_unpin_toggles() {
        let (tool, store) = setup().await;
        tool.execute(serde_json::json!({"action": "pin", "skill": "foo"}))
            .await
            .unwrap();
        assert!(store.get("foo").await.unwrap().pinned);
        tool.execute(serde_json::json!({"action": "unpin", "skill": "foo"}))
            .await
            .unwrap();
        assert!(!store.get("foo").await.unwrap().pinned);
    }

    #[tokio::test]
    async fn archive_unknown_skill_errors_with_available() {
        let (tool, _) = setup().await;
        let out = tool
            .execute(serde_json::json!({"action": "archive", "skill": "nope"}))
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("foo"));
    }

    #[tokio::test]
    async fn action_requires_skill_name() {
        let (tool, _) = setup().await;
        let out = tool
            .execute(serde_json::json!({"action": "archive"}))
            .await
            .unwrap();
        assert!(!out.success);
    }
}
