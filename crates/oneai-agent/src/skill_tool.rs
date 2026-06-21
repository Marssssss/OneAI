//! `skill` tool — progressive disclosure Tier2/Tier3.
//!
//! The model invokes this tool to load a skill's full prompt. The list of
//! available skills (name + description) is injected into the system prompt
//! every turn by the AgentLoop (Tier1 menu). When the model decides a skill is
//! relevant, it calls this tool with the skill name; the tool returns the
//! skill's `prompt_template` as its result so the model follows it for the
//! remainder of the task. This mirrors Claude Code's `Skill` tool.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::Tool;
use oneai_core::{RiskLevel, ToolOutput};
use oneai_skill::SkillRegistry;

/// A tool that loads a skill's full prompt on demand.
///
/// Holds a shared `Arc<SkillRegistry>` — the same registry the AgentLoop reads
/// to build the always-on skill menu, so the model can only invoke skills that
/// are actually registered.
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
}

impl SkillTool {
    /// Create a new `skill` tool backed by the given registry.
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load and activate a named skill's full instructions. Only invoke skills listed in the \
        system prompt's 'Available skills' section. Pass the exact skill name. The tool returns the \
        skill's detailed prompt — follow it for the rest of the task."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The exact name of the skill to activate (from the Available skills list)."
                },
                "args": {
                    "type": "string",
                    "description": "Optional free-form arguments to pass to the skill."
                }
            },
            "required": ["skill"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let skill_name = args.get("skill").and_then(|v| v.as_str()).unwrap_or("");

        if skill_name.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No skill name provided. Use a name from the 'Available skills' list.".to_string()),
            });
        }

        match self.registry.find_by_name(skill_name).await {
            Some(skill) => {
                tracing::info!("SkillTool: activating skill '{}'", skill_name);
                // Return the full prompt_template — the model reads it and follows it.
                // Including the name + description helps the model confirm what it loaded.
                let content = format!(
                    "# Skill activated: {}\n{}\n\n---\nFollow these instructions for this skill. \
                     Description: {}",
                    skill.name, skill.prompt_template, skill.description
                );
                Ok(ToolOutput {
                    success: true,
                    content,
                    error: None,
                })
            }
            None => {
                // Help the model recover: list available skills so it can retry.
                let available = self.registry.skill_names().await;
                let available_str = if available.is_empty() {
                    "(none registered)".to_string()
                } else {
                    available.join(", ")
                };
                Ok(ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(format!(
                        "Skill '{}' not found. Available skills: {}",
                        skill_name, available_str
                    )),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::SkillDescriptor;

    fn sample_skill() -> SkillDescriptor {
        SkillDescriptor {
            name: "creative-writing".into(),
            description: "Generate creative content".into(),
            prompt_template: "You are a creative writing expert.".into(),
            trigger_keywords: vec!["write".into()],
            embedding: None,
        }
    }

    #[tokio::test]
    async fn test_skill_tool_loads_prompt() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(sample_skill()).await.unwrap();
        let tool = SkillTool::new(registry);

        let out = tool.execute(serde_json::json!({"skill": "creative-writing"})).await.unwrap();
        assert!(out.success);
        assert!(out.content.contains("creative writing expert"));
        assert!(out.content.contains("Skill activated: creative-writing"));
    }

    #[tokio::test]
    async fn test_skill_tool_unknown_lists_available() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(sample_skill()).await.unwrap();
        let tool = SkillTool::new(registry);

        let out = tool.execute(serde_json::json!({"skill": "nope"})).await.unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("creative-writing"));
    }

    #[tokio::test]
    async fn test_skill_tool_missing_name() {
        let registry = Arc::new(SkillRegistry::new());
        let tool = SkillTool::new(registry);

        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!out.success);
    }
}
