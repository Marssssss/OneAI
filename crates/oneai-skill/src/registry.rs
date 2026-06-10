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
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}