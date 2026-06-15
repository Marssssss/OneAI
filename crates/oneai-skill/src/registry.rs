//! Skill registry — management of registered skills.

use std::collections::HashMap;
use tokio::sync::RwLock;
use oneai_core::SkillDescriptor;
use oneai_core::error::Result;

/// Registry for managing skills.
pub struct SkillRegistry {
    skills: RwLock<HashMap<String, SkillDescriptor>>,
}

impl SkillRegistry {
    /// Create a new empty skill registry.
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
        }
    }

    /// Register a skill.
    pub async fn register(&self, skill: SkillDescriptor) -> Result<()> {
        let mut skills = self.skills.write().await;
        skills.insert(skill.name.clone(), skill);
        Ok(())
    }

    /// Remove a skill by name.
    pub async fn remove(&self, name: &str) -> Result<()> {
        let mut skills = self.skills.write().await;
        skills.remove(name);
        Ok(())
    }

    /// List all registered skills.
    pub async fn list(&self) -> Vec<SkillDescriptor> {
        let skills = self.skills.read().await;
        skills.values().cloned().collect()
    }

    /// Register multiple built-in skills at once.
    ///
    /// Typically called with `builtin::coding_skills()`, `builtin::research_skills()`,
    /// or `builtin::skills_for_domain("coding")`.
    pub async fn register_builtin(&self, skills: Vec<SkillDescriptor>) -> Result<()> {
        let mut map = self.skills.write().await;
        for skill in skills {
            map.insert(skill.name.clone(), skill);
        }
        Ok(())
    }

    /// Find a skill by its exact name.
    ///
    /// Returns `None` if the skill is not registered.
    pub async fn find_by_name(&self, name: &str) -> Option<SkillDescriptor> {
        let skills = self.skills.read().await;
        skills.get(name).cloned()
    }

    /// Get all registered skill names (sorted alphabetically).
    ///
    /// Used by the TUI sidebar to display the skill list.
    pub async fn skill_names(&self) -> Vec<String> {
        let skills = self.skills.read().await;
        let mut names: Vec<String> = skills.keys().cloned().collect();
        names.sort();
        names
    }

    /// Get the total number of registered skills.
    pub async fn count(&self) -> usize {
        let skills = self.skills.read().await;
        skills.len()
    }

    /// Clear all registered skills.
    ///
    /// Called when switching domains to remove the old domain's skills
    /// before registering the new domain's skills.
    pub async fn clear(&self) {
        let mut skills = self.skills.write().await;
        skills.clear();
    }

    /// Replace all skills with a new set (clear + register).
    ///
    /// Convenience method for domain switching.
    pub async fn replace_all(&self, skills: Vec<SkillDescriptor>) -> Result<()> {
        self.clear().await;
        self.register_builtin(skills).await?;
        Ok(())
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}
