//! Skill selector — lightweight top-K selection with progressive disclosure.

use oneai_core::{SkillDescriptor, SelectionMode};
use oneai_core::error::Result;

/// Lightweight skill selector that dynamically injects relevant skills into context.
///
/// Uses keyword matching or lightweight vector similarity to select top-K skills
/// from the registry. Progressive disclosure: only the most relevant skill
/// descriptions are injected; previous skills auto-unload when topic changes.
pub struct SkillSelector {
    /// Selection mode (keyword, vector, or hybrid).
    mode: SelectionMode,
    /// Number of top skills to select (default: 3).
    top_k: usize,
}

impl SkillSelector {
    /// Create a new skill selector with default settings.
    pub fn new() -> Self {
        Self {
            mode: SelectionMode::KeywordMatch,
            top_k: 3,
        }
    }

    /// Create a skill selector with a specific mode and top-K.
    pub fn with_config(mode: SelectionMode, top_k: usize) -> Self {
        Self { mode, top_k }
    }

    /// Select the most relevant skills for a user input.
    pub async fn select_skills(
        &self,
        user_input: &str,
        registry: &[SkillDescriptor],
    ) -> Result<Vec<SkillDescriptor>> {
        // Keyword matching implementation
        let scored = registry.iter().map(|skill| {
            let keyword_score = skill.trigger_keywords.iter().map(|kw| {
                if oneai_core::keyword_matches(user_input, kw) {
                    1.0
                } else {
                    0.0
                }
            }).sum::<f32>() / skill.trigger_keywords.len().max(1) as f32;
            (skill, keyword_score)
        }).collect::<Vec<_>>();

        let mut sorted = scored;
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        Ok(sorted.into_iter()
            .take(self.top_k)
            .filter(|(_, score)| *score > 0.0)
            .map(|(skill, _)| skill.clone())
            .collect())
    }
}

impl Default for SkillSelector {
    fn default() -> Self {
        Self::new()
    }
}