//! CompressionTemplate — domain-specific context preservation priorities.
//!
//! The CompressionTemplate defines how conversation context should be compressed
//! when it exceeds the token budget. Different domains have different preservation
//! priorities:
//!
//! - Coding: critical files > progress status > key decisions > code snippets
//! - Research: source URLs > key findings > arguments > raw data
//! - Data analysis: query results > conclusions > data characteristics > SQL
//!
//! The template provides:
//! - A structured summarization prompt (replaces the generic one)
//! - Truncation rules for different content types
//! - Fields that must be preserved during compression

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ─── CompressionTemplate ───────────────────────────────────────────────────────

/// Domain-specific context compression configuration.
///
/// When the conversation exceeds the token budget, the ContextCompressor
/// uses this template to produce a structured summary that preserves the
/// most important information for the domain.
///
/// Without a template, the compressor uses a generic summarization prompt
/// that produces a simple paragraph. With a template, it produces a
/// structured summary following the domain's priorities.
///
/// Example (CodingPack):
/// ```text
/// # Session Summary
/// ## Goal: {{task_description}}
/// ## Progress:
///   - ✅ Step 1: {{step_1_status}}
///   - 🔄 Step 2: {{step_2_status}}
/// ## Key Decisions: {{key_decisions}}
/// ## Critical Files: {{critical_files}}
/// ## Next Steps: {{next_steps}}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompressionTemplate {
    /// Human-readable name for this template (e.g., "coding", "research").
    pub name: String,

    /// Fields that must be preserved during compression.
    ///
    /// The compressor ensures these fields are always included in the summary,
    /// even if they take up more tokens than other content.
    ///
    /// Examples:
    /// - Coding: ["critical_files", "progress_status", "key_decisions", "next_steps"]
    /// - Research: ["source_urls", "key_findings", "arguments", "citations"]
    pub preserve_fields: Vec<String>,

    /// The structured template for the summarization prompt.
    ///
    /// Uses {{variable}} interpolation syntax. The compressor fills in
    /// variables from the conversation before sending to the LLM.
    ///
    /// Default variables available:
    /// - {{task_description}} — the original user task
    /// - {{conversation_summary}} — a brief summary of what happened
    /// - {{preserve_hint}} — instruction to preserve specific fields
    pub template: String,

    /// Truncation rules: content type → max characters.
    ///
    /// Before summarization, the compressor truncates long content
    /// according to these rules. This prevents the summarization prompt
    /// from being too large.
    ///
    /// Examples:
    /// - Coding: {"tool_output": 2000, "file_content": 5000}
    /// - Research: {"web_content": 3000, "pdf_content": 5000}
    pub truncate_rules: HashMap<String, usize>,

    /// Default variables for the template.
    ///
    /// These are always available during interpolation, in addition to
    /// the dynamically extracted variables.
    pub default_variables: HashMap<String, String>,
}

impl CompressionTemplate {
    /// Create a new compression template with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            preserve_fields: Vec::new(),
            template: String::new(),
            truncate_rules: HashMap::new(),
            default_variables: HashMap::new(),
        }
    }

    /// Build the full system prompt for the LLM summarization request.
    ///
    /// Combines the template with preserve_field hints to produce
    /// the complete instruction for the summarizer.
    pub fn build_summarization_prompt(&self, task_description: &str) -> String {
        let preserve_hint = if self.preserve_fields.is_empty() {
            String::new()
        } else {
            format!(
                "\nIMPORTANT: You MUST preserve the following information in your summary:\n{}\n\
                Do not omit or abbreviate these fields — they are critical for continuing the task.",
                self.preserve_fields.iter()
                    .map(|f| format!("  - {}", f))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };

        if self.template.is_empty() {
            // No template → use generic prompt + preserve hint
            format!(
                "You are a conversation summarizer. Summarize the conversation below \
                into a concise summary that captures the key facts, decisions, and \
                context needed to continue the conversation.{}\n\nTask: {}",
                preserve_hint, task_description
            )
        } else {
            // Has template → use structured prompt
            let mut prompt = self.template.clone();

            // Fill in default variables
            for (key, value) in &self.default_variables {
                prompt = prompt.replace(&format!("{{{{{}}}}}", key), value);
            }

            // Fill in task description
            prompt = prompt.replace("{{task_description}}", task_description);

            // Add preserve hint
            format!("{}{}", prompt, preserve_hint)
        }
    }

    /// Truncate a content string according to truncate_rules.
    ///
    /// If the content type has a truncation rule, truncate to that size.
    /// Otherwise, return the content unchanged.
    pub fn truncate_content(&self, content_type: &str, content: &str) -> String {
        if let Some(max_chars) = self.truncate_rules.get(content_type) {
            if content.len() > *max_chars {
                let mut truncated = content[..*max_chars].to_string();
                truncated.push_str("\n... [truncated due to domain compression policy]");
                truncated
            } else {
                content.to_string()
            }
        } else {
            content.to_string()
        }
    }
}

impl Default for CompressionTemplate {
    fn default() -> Self {
        Self::new("default")
    }
}

// ─── Coding Compression Template ───────────────────────────────────────────────

/// The coding domain compression template.
///
/// Preserves the most critical information for continuing coding work:
/// - Critical file paths (where the edits are happening)
/// - Progress status (which steps are done)
/// - Key decisions (architecture choices made)
/// - Next steps (what to do next)
pub const CODING_COMPRESSION_TEMPLATE: &str = "\
You are a coding session summarizer. Summarize the conversation below into a \
structured summary following this exact format:

# Session Summary
## Goal: {{task_description}}
## Constraints: {{constraints}}
## Progress:
  For each task step, list status (✅ completed / 🔄 in progress / ⏳ pending / ❌ failed)
## Key Decisions: List all important architectural or design decisions made
## Critical Files: List all file paths that were read, modified, or are relevant
## Next Steps: What should be done next to continue the task
## Errors/Issues: Any errors encountered and their resolution status

Be concise but complete. Do NOT omit file paths or progress status — these are \
critical for continuing coding work.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_template_default() {
        let template = CompressionTemplate::default();
        assert_eq!(template.name, "default");
        assert!(template.preserve_fields.is_empty());
        assert!(template.template.is_empty());
    }

    #[test]
    fn test_build_summarization_prompt_no_template() {
        let template = CompressionTemplate {
            name: "test".to_string(),
            preserve_fields: vec!["critical_files".to_string(), "progress_status".to_string()],
            template: String::new(),
            truncate_rules: HashMap::new(),
            default_variables: HashMap::new(),
        };

        let prompt = template.build_summarization_prompt("Refactor auth module");
        assert!(prompt.contains("Refactor auth module"));
        assert!(prompt.contains("critical_files"));
        assert!(prompt.contains("progress_status"));
    }

    #[test]
    fn test_build_summarization_prompt_with_template() {
        let template = CompressionTemplate {
            name: "coding".to_string(),
            preserve_fields: vec!["critical_files".to_string()],
            template: "Summarize for coding. Task: {{task_description}}".to_string(),
            truncate_rules: HashMap::new(),
            default_variables: HashMap::new(),
        };

        let prompt = template.build_summarization_prompt("Fix bug in login");
        assert!(prompt.contains("Fix bug in login"));
        assert!(prompt.contains("critical_files"));
    }

    #[test]
    fn test_truncate_content() {
        let template = CompressionTemplate {
            name: "coding".to_string(),
            preserve_fields: Vec::new(),
            template: String::new(),
            truncate_rules: HashMap::from([
                ("tool_output".to_string(), 100),
                ("file_content".to_string(), 500),
            ]),
            default_variables: HashMap::new(),
        };

        let short = "hello world";
        assert_eq!(template.truncate_content("tool_output", short), short);

        let long = "a".repeat(200);
        let truncated = template.truncate_content("tool_output", &long);
        assert!(truncated.len() < 200);
        assert!(truncated.contains("truncated"));

        let no_rule = "some long content".repeat(100);
        assert_eq!(template.truncate_content("unknown_type", &no_rule), no_rule);
    }

    #[test]
    fn test_coding_template_content() {
        assert!(CODING_COMPRESSION_TEMPLATE.contains("Session Summary"));
        assert!(CODING_COMPRESSION_TEMPLATE.contains("Critical Files"));
        assert!(CODING_COMPRESSION_TEMPLATE.contains("Progress"));
    }
}
