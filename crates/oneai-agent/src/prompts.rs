//! Prompt template system — variable interpolation and model-specific overrides.
//!
//! This addresses Issue #20: all prompts are hardcoded string constants
//! (PLAN_SYSTEM_PROMPT, REFLECTION_SYSTEM_PROMPT, ReActConfig.system_prompt).
//! DSPy's insight is that manually written prompts are fragile — they should
//! be templates that can be optimized by algorithms or adapted per model.
//!
//! The PromptTemplate system supports:
//! - Variable interpolation with {{variable}} syntax
//! - Model-specific overrides (shorter prompts for small models)
//! - Version management (track prompt changes over time)
//! - Runtime selection (choose template based on model/task)

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ─── PromptTemplate ─────────────────────────────────────────────────────────

/// A prompt template — supports {{variable}} interpolation and version management.
///
/// Templates replace hardcoded string constants like PLAN_SYSTEM_PROMPT.
/// Each template has:
/// - A unique name (e.g., "plan_system", "react_system")
/// - A template string with {{variable}} placeholders
/// - Default values for variables
/// - A version tag for tracking changes
///
/// Example usage:
/// ```rust,no_run
/// let template = oneai_agent::PromptTemplate::new(
///     "plan_system",
///     "You are a {{role}} assistant.",
/// );
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplate {
    /// The template name (unique identifier).
    pub name: String,

    /// The template string with {{variable}} placeholders.
    pub template: String,

    /// Default values for template variables.
    #[serde(default)]
    pub variables: HashMap<String, String>,

    /// Version tag (for tracking prompt changes).
    #[serde(default)]
    pub version: String,

    /// Description of the template's purpose.
    #[serde(default)]
    pub description: String,
}

impl PromptTemplate {
    /// Create a new prompt template.
    pub fn new(name: impl Into<String>, template: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            template: template.into(),
            variables: HashMap::new(),
            version: "1.0".to_string(),
            description: String::new(),
        }
    }

    /// Create with default variables.
    pub fn with_defaults(name: impl Into<String>, template: impl Into<String>, variables: HashMap<String, String>) -> Self {
        Self {
            name: name.into(),
            template: template.into(),
            variables,
            version: "1.0".to_string(),
            description: String::new(),
        }
    }

    /// Render the template with the provided variables.
    ///
    /// Replaces all {{variable}} placeholders with their values.
    /// Variables not provided fall back to defaults.
    /// Placeholders with no value are left as-is.
    pub fn render(&self, overrides: &HashMap<String, String>) -> String {
        let mut result = self.template.clone();

        // First apply default variables
        for (key, value) in &self.variables {
            result = result.replace(&format!("{{{{{}}}}}", key), value);
        }

        // Then apply overrides (which take precedence)
        for (key, value) in overrides {
            result = result.replace(&format!("{{{{{}}}}}", key), value);
        }

        result
    }

    /// Render with only default variables (no overrides).
    pub fn render_defaults(&self) -> String {
        self.render(&HashMap::new())
    }
}

// ─── PromptRegistry ────────────────────────────────────────────────────────

/// Prompt registry — stores templates and provides model-specific selection.
///
/// The registry:
/// 1. Stores all prompt templates by name
/// 2. Allows model-specific overrides (e.g., shorter prompts for small models)
/// 3. Selects the appropriate template at runtime based on the model name
///
/// Usage:
/// ```ignore
/// let registry = PromptRegistry::new();
/// registry.register("plan_system", plan_template);
/// registry.register_model_override("plan_system", "llama-7b", shorter_plan_template);
///
/// // At runtime, select template based on model:
/// let prompt = registry.get("plan_system", "llama-7b");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRegistry {
    /// Default templates (used when no model override is available).
    templates: HashMap<String, PromptTemplate>,

    /// Model-specific template overrides.
    /// Key: (template_name, model_name) → overridden template.
    #[serde(default)]
    model_overrides: HashMap<String, HashMap<String, PromptTemplate>>,
}

impl PromptRegistry {
    /// Create a new empty prompt registry.
    pub fn new() -> Self {
        Self {
            templates: HashMap::new(),
            model_overrides: HashMap::new(),
        }
    }

    /// Create a registry with the default OneAI prompts.
    ///
    /// Includes:
    /// - "plan_system" — task decomposition prompt
    /// - "react_system" — tool-calling agent prompt
    /// - "reflection_system" — result verification prompt
    /// - "explore_system" — search/understand prompt
    /// - "loop_system" — Agentic Loop decision-making prompt
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();

        // Plan system prompt
        registry.register(PromptTemplate::with_defaults(
            "plan_system",
            "You are a {{role}} assistant. When given a {{task_type}}, decompose it into clear, ordered steps.\n\n\
             For each step, determine whether it is:\n\
             - COUPLED: This step depends on the output of a previous step.\n\
             - NON-COULED: This step can be executed independently.\n\n\
             Output your plan as a JSON array with this exact format:\n\
             ```json\n\
             [{\"id\": \"step_1\", \"description\": \"Brief description\", \"coupled\": false, \"depends_on\": []}]\n\
             ```\n\n\
             Important: Output ONLY the JSON array, no other text.",
            HashMap::from([
                ("role".to_string(), "task planning".to_string()),
                ("task_type".to_string(), "complex task".to_string()),
            ]),
        ));

        // ReAct system prompt
        registry.register(PromptTemplate::with_defaults(
            "react_system",
            "You are a {{role}} AI assistant that can use tools to accomplish tasks.\n\
             When you need to use a tool, output a tool call.\n\
             When you have the final answer, respond with just text without any tool calls.\n\
             {{additional_instructions}}",
            HashMap::from([
                ("role".to_string(), "helpful".to_string()),
                ("additional_instructions".to_string(), "".to_string()),
            ]),
        ));

        // Reflection system prompt
        registry.register(PromptTemplate::with_defaults(
            "reflection_system",
            "You are a result verification assistant. Evaluate whether a given result \
             accurately addresses the original task.\n\n\
             Evaluate on: ACCURACY, COMPLETENESS, RELEVANCE.\n\n\
             Output as JSON: {\"passed\": true/false, \"confidence\": 0.0-1.0, \
             \"issues\": [...], \"suggestions\": [...]}",
            HashMap::new(),
        ));

        // Agentic Loop decision prompt
        registry.register(PromptTemplate::with_defaults(
            "loop_system",
            "You are an intelligent AI agent that can plan, execute, and reflect on tasks.\n\
             When you need to use a tool, output a tool call.\n\
             When you have the final answer, respond with just text.\n\
             When a task is complex, you can delegate it to a specialized sub-agent.",
            HashMap::new(),
        ));

        registry
    }

    /// Register a prompt template.
    pub fn register(&mut self, template: PromptTemplate) {
        self.templates.insert(template.name.clone(), template);
    }

    /// Register a model-specific override for a template.
    ///
    /// When `get(template_name, model_name)` is called, if there's
    /// a model override for that model, it's returned instead of
    /// the default template. This allows:
    /// - Shorter prompts for small models (Llama 7B, etc.)
    /// - More detailed prompts for large models (GPT-4, Claude)
    /// - Language-specific prompts for multilingual models
    pub fn register_model_override(
        &mut self,
        template_name: impl Into<String>,
        model_name: impl Into<String>,
        override_template: PromptTemplate,
    ) {
        self.model_overrides
            .entry(template_name.into())
            .or_default()
            .insert(model_name.into(), override_template);
    }

    /// Get a prompt template, selecting model-specific override if available.
    ///
    /// Selection logic:
    /// 1. If there's a model override for the exact model name → use it
    /// 2. If there's a model family override (e.g., "llama" for "llama-7b") → use it
    /// 3. Otherwise → use the default template
    pub fn get(&self, template_name: &str, model_name: &str) -> Option<&PromptTemplate> {
        // Try exact model name override
        if let Some(overrides) = self.model_overrides.get(template_name) {
            if let Some(override_template) = overrides.get(model_name) {
                return Some(override_template);
            }

            // Try model family prefix (e.g., "llama" matches "llama-7b-hf")
            for (override_model, override_template) in overrides {
                if model_name.starts_with(override_model) {
                    return Some(override_template);
                }
            }
        }

        // Fall back to default template
        self.templates.get(template_name)
    }

    /// Get a prompt template with only default variables (no model context).
    pub fn get_default(&self, template_name: &str) -> Option<&PromptTemplate> {
        self.templates.get(template_name)
    }

    /// List all registered template names.
    pub fn template_names(&self) -> Vec<String> {
        self.templates.keys().cloned().collect()
    }
}

impl Default for PromptRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}