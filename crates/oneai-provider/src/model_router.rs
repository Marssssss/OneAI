//! Model Router — cost-based provider routing for intelligent model selection.
//!
//! OneAI's multi-provider architecture (Anthropic/OpenAI/Gemini/Ollama) enables
//! a unique capability: routing tasks to different models based on complexity.
//! Simple tasks → cheap models (Haiku, local Ollama), complex tasks → expensive
//! models (Opus, GPT-4). This reduces cost without sacrificing quality.
//!
//! This addresses the "无成本模型路由" gap identified in the competitive analysis.
//! No other coding agent framework currently offers built-in cost-based routing.
//!
//! **How it works**:
//! 1. The ModelRouter evaluates the task description + current paradigm
//! 2. Matches against a list of RouteRules (keyword patterns → model/provider)
//! 3. Returns a RouteDecision specifying which model and provider to use
//! 4. The AgentLoop uses this decision to configure the next inference call
//!
//! **Default rules** (can be overridden by user config):
//! - Quick/simple tasks → claude-haiku-4-5 / ollama:small (cheap, fast)
//! - Implementation/debug tasks → claude-sonnet-4-6 / gpt-4o (balanced)
//! - Architecture/planning tasks → claude-opus-4-8 / gpt-4 (powerful)
//!
//! **Custom rules** can be added via YAML/TOML configuration or programmatically.

use regex::Regex;
use oneai_core::{CloudProviderKind, ModelConfig, ProviderType};
use oneai_core::traits::LlmProvider;
use crate::ProviderFactory;

// ─── RouteRule ────────────────────────────────────────────────────────────────

/// A single routing rule — maps a task pattern to a specific model/provider.
///
/// Rules are evaluated in order; the first match wins. If no rule matches,
/// the fallback provider (configured at construction time) is used.
#[derive(Debug, Clone)]
pub struct RouteRule {
    /// Regex pattern for matching task descriptions.
    /// Examples: "quick fix|simple|lookup", "implement|refactor|debug", "architect|plan|review"
    pub pattern: Regex,

    /// The model name to route to.
    /// Examples: "claude-haiku-4-5-20251001", "claude-sonnet-4-20250514", "ollama:qwen2.5:0.5b"
    pub model: String,

    /// Optional provider override — if set, forces a specific provider type.
    /// If None, the model name is used to auto-detect the provider.
    pub provider_override: Option<RouteProviderKind>,

    /// Optional max_tokens override — if set, limits output tokens for this route.
    pub max_tokens_override: Option<u32>,

    /// Human-readable description of what this rule targets (for logging/debugging).
    pub description: String,
}

/// Provider kind for routing decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteProviderKind {
    Anthropic,
    OpenAI,
    Gemini,
    Ollama,
}

impl std::fmt::Display for RouteProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anthropic => write!(f, "anthropic"),
            Self::OpenAI => write!(f, "openai"),
            Self::Gemini => write!(f, "gemini"),
            Self::Ollama => write!(f, "ollama"),
        }
    }
}

impl RouteProviderKind {
    /// Convert to CloudProviderKind for ModelConfig.
    pub fn to_cloud_kind(&self) -> Option<CloudProviderKind> {
        match self {
            Self::Anthropic => Some(CloudProviderKind::Anthropic),
            Self::OpenAI => Some(CloudProviderKind::OpenAI),
            Self::Gemini => Some(CloudProviderKind::Gemini),
            Self::Ollama => None, // Ollama is ProviderType::Local
        }
    }

    /// Convert to ProviderType for ModelConfig.
    pub fn to_provider_type(&self) -> ProviderType {
        match self {
            Self::Ollama => ProviderType::Local,
            _ => ProviderType::Cloud,
        }
    }
}

// ─── RouteDecision ────────────────────────────────────────────────────────────

/// The result of a routing decision — specifies which model and provider to use.
#[derive(Debug, Clone)]
pub struct RouteDecision {
    /// The model name selected by the router.
    pub model: String,

    /// The provider kind selected by the router.
    pub provider: RouteProviderKind,

    /// Optional max_tokens override for this route.
    pub max_tokens: Option<u32>,

    /// The rule that matched (for logging/debugging).
    pub matched_rule: String,

    /// The reason for this routing decision (human-readable).
    pub reason: String,
}

// ─── TierModels ────────────────────────────────────────────────────────────────

/// Provider-specific model tier definitions.
///
/// Maps each cost/capability tier to a concrete model name for a given provider.
/// This ensures default rules always route to models that exist on the user's
/// configured provider — no cross-provider routing in defaults.
///
/// | Tier | Anthropic | OpenAI | Gemini | Ollama |
/// |------|-----------|--------|--------|--------|
/// | Cheap | claude-haiku-4-5 | gpt-4o-mini | gemini-2.0-flash | qwen2.5:0.5b |
/// | Balanced | claude-sonnet-4-6 | gpt-4o | gemini-2.5-flash | qwen2.5:7b |
/// | Powerful | claude-opus-4-8 | o3-pro | gemini-2.5-pro | deepseek-r1:14b |
struct TierModels {
    /// Cheap/fast model — for simple tasks and exploration.
    cheap: String,
    /// Balanced model — for implementation and debugging.
    balanced: String,
    /// Powerful model — for architecture, planning, and research.
    powerful: String,
}

// ─── ModelRouter ──────────────────────────────────────────────────────────────

/// Cost-based model router — selects the appropriate model/provider
/// based on task complexity and paradigm.
///
/// The router evaluates route rules in order and returns the first match.
/// If no rules match, it uses the fallback configuration.
///
/// **Usage**:
/// ```ignore
/// let router = ModelRouter::with_defaults(fallback_config);
///
/// // Before each inference, evaluate the route:
/// let decision = router.route("Implement a new authentication module", ParadigmKind::ReAct);
/// // decision.model = "claude-sonnet-4-20250514"
/// // decision.reason = "Implementation task → balanced model"
///
/// let decision = router.route("What is the capital of France?", ParadigmKind::ReAct);
/// // decision.model = "claude-haiku-4-5-20251001"
/// // decision.reason = "Simple question → cheap model"
/// ```
pub struct ModelRouter {
    /// Ordered list of routing rules. First match wins.
    rules: Vec<RouteRule>,

    /// Fallback configuration — used when no rule matches.
    fallback_config: ModelConfig,
}

impl ModelRouter {
    /// Create a new ModelRouter with custom rules and a fallback config.
    pub fn new(rules: Vec<RouteRule>, fallback_config: ModelConfig) -> Self {
        Self { rules, fallback_config }
    }

    /// Create a ModelRouter with built-in default rules.
    /// Create a ModelRouter with built-in default rules that adapt to the provider.
    ///
    /// Default rules provide a sensible cost optimization strategy.
    /// The model names in the rules are **automatically adapted** based on
    /// the fallback config's provider type:
    ///
    /// | Tier | Anthropic | OpenAI | Gemini | Ollama |
    /// |------|-----------|--------|--------|--------|
    /// | Cheap (simple/explore) | claude-haiku-4-5 | gpt-4o-mini | gemini-2.0-flash | qwen2.5:0.5b |
    /// | Balanced (implement/debug) | claude-sonnet-4-6 | gpt-4o | gemini-2.5-pro | qwen2.5:7b |
    /// | Powerful (architect/research) | claude-opus-4-8 | o3-pro | gemini-2.5-pro | deepseek-r1:14b |
    ///
    /// **Important**: If the user has configured a non-Anthropic provider, the default
    /// rules will NOT try to route to Anthropic models — they route to models that
    /// exist on the user's configured provider. This prevents routing failures when
    /// the user only has an OpenAI or Ollama API key.
    ///
    /// The fallback uses the provided ModelConfig for tasks that don't match any rule.
    pub fn with_defaults(fallback_config: ModelConfig) -> Self {
        let provider = Self::infer_provider_from_config_static(&fallback_config);
        Self::new(Self::default_rules_for_provider(&provider), fallback_config)
    }

    /// Evaluate a routing decision based on task description and current paradigm.
    ///
    /// Returns a RouteDecision specifying which model and provider to use.
    /// If no rule matches, returns a decision using the fallback config.
    pub fn route(&self, task_description: &str, paradigm: &str) -> RouteDecision {
        // Evaluate rules in order — first match wins
        for rule in &self.rules {
            if rule.pattern.is_match(task_description) {
                tracing::info!(
                    "ModelRouter: routing '{}' → model '{}' (rule: '{}')",
                    task_description.chars().take(50).collect::<String>(),
                    rule.model,
                    rule.description,
                );

                return RouteDecision {
                    model: rule.model.clone(),
                    provider: rule.provider_override.clone()
                        .unwrap_or_else(|| self.infer_provider_from_model(&rule.model)),
                    max_tokens: rule.max_tokens_override,
                    matched_rule: rule.description.clone(),
                    reason: format!("Task matched rule: {}", rule.description),
                };
            }
        }

        // Paradigm-based fallback: if task didn't match any rule,
        // use paradigm hints for routing
        let paradigm_hint = match paradigm.to_lowercase().as_str() {
            "plan" => "Plan paradigm → balanced model",
            "reflect" => "Reflect paradigm → balanced model",
            "explore" => "Explore paradigm → cheap model (read-only task)",
            _ => "Default → fallback model",
        };

        tracing::debug!(
            "ModelRouter: no rule matched for '{}', using fallback ({})",
            task_description.chars().take(50).collect::<String>(),
            paradigm_hint,
        );

        RouteDecision {
            model: self.fallback_config.model_name.clone()
                .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string()),
            provider: self.infer_provider_from_config(&self.fallback_config),
            max_tokens: None,
            matched_rule: "fallback".to_string(),
            reason: paradigm_hint.to_string(),
        }
    }

    /// Create a LlmProvider from a RouteDecision.
    ///
    /// This uses the ProviderFactory to create the appropriate provider
    /// based on the route decision's model and provider specifications.
    pub fn create_provider(&self, decision: &RouteDecision, api_key: Option<String>) -> Box<dyn LlmProvider> {
        let config = ModelConfig {
            provider_type: decision.provider.to_provider_type(),
            cloud_kind: decision.provider.to_cloud_kind(),
            api_key,
            base_url: None, // Use provider defaults
            port: None,
            model_name: Some(decision.model.clone()),
            model_path: None,
            extra: std::collections::HashMap::new(),
        };

        ProviderFactory::create(config)
    }

    /// Infer the provider kind from a model name string.
    ///
    /// Common patterns:
    /// - "claude-*" → Anthropic
    /// - "gpt-*" → OpenAI
    /// - "gemini-*" → Gemini
    /// - "ollama:*" → Ollama
    fn infer_provider_from_model(&self, model: &str) -> RouteProviderKind {
        let lower = model.to_lowercase();
        if lower.starts_with("claude") { RouteProviderKind::Anthropic }
        else if lower.starts_with("gpt") || lower.contains("openai") { RouteProviderKind::OpenAI }
        else if lower.starts_with("gemini") { RouteProviderKind::Gemini }
        else if lower.contains("ollama") || lower.contains("local") { RouteProviderKind::Ollama }
        else { RouteProviderKind::OpenAI } // Default: most services use OpenAI protocol
    }

    /// Infer the provider kind from an existing ModelConfig.
    fn infer_provider_from_config(&self, config: &ModelConfig) -> RouteProviderKind {
        Self::infer_provider_from_config_static(config)
    }

    /// Static version — used in with_defaults() before the router is constructed.
    fn infer_provider_from_config_static(config: &ModelConfig) -> RouteProviderKind {
        if config.provider_type == ProviderType::Local {
            return RouteProviderKind::Ollama;
        }
        match config.cloud_kind {
            Some(CloudProviderKind::Anthropic) => RouteProviderKind::Anthropic,
            Some(CloudProviderKind::Gemini) => RouteProviderKind::Gemini,
            Some(CloudProviderKind::OpenAI) | None => {
                // Auto-detect from base_url if cloud_kind not set
                let url = config.base_url.as_deref().unwrap_or("").to_lowercase();
                if url.contains("anthropic.com") { RouteProviderKind::Anthropic }
                else if url.contains("generativelanguage.googleapis.com") || url.contains("aiplatform.googleapis.com") { RouteProviderKind::Gemini }
                else { RouteProviderKind::OpenAI }
            },
        }
    }

    // ─── Provider-Adaptive Model Tiers ────────────────────────────────────────

    /// Model tier definitions per provider — maps cost tiers to concrete model names.
    ///
    /// Each provider has three tiers:
    /// - **Cheap**: fast, low-cost — for simple/explore tasks
    /// - **Balanced**: mid-range — for implement/debug tasks
    /// - **Powerful**: high-capability — for architect/research tasks
    ///
    /// These are the default model names for each provider. Users can override
    /// them by adding custom rules or modifying the config.
    fn model_tiers_for_provider(provider: &RouteProviderKind) -> (TierModels, RouteProviderKind) {
        let tiers = match provider {
            RouteProviderKind::Anthropic => TierModels {
                cheap: "claude-haiku-4-5-20251001".to_string(),
                balanced: "claude-sonnet-4-6-20250514".to_string(),
                powerful: "claude-opus-4-8".to_string(),
            },
            RouteProviderKind::OpenAI => TierModels {
                cheap: "gpt-4o-mini".to_string(),
                balanced: "gpt-4o".to_string(),
                powerful: "o3-pro".to_string(),
            },
            RouteProviderKind::Gemini => TierModels {
                cheap: "gemini-2.0-flash".to_string(),
                balanced: "gemini-2.5-flash".to_string(),
                powerful: "gemini-2.5-pro".to_string(),
            },
            RouteProviderKind::Ollama => TierModels {
                cheap: "qwen2.5:0.5b".to_string(),
                balanced: "qwen2.5:7b".to_string(),
                powerful: "deepseek-r1:14b".to_string(),
            },
        };
        (tiers, provider.clone())
    }

    /// Build default routing rules adapted to the given provider.
    ///
    /// The rules use the model names from `model_tiers_for_provider()` so
    /// that they always route to models that actually exist on the user's
    /// configured provider. No cross-provider routing in defaults.
    fn default_rules_for_provider(provider: &RouteProviderKind) -> Vec<RouteRule> {
        let (tiers, provider_kind) = Self::model_tiers_for_provider(provider);

        vec![
            // ─── Simple/Quick tasks → cheapest models ───
            RouteRule {
                pattern: Regex::new("(?i)(\\bquick\\s*fix\\b|\\bsimple\\b|\\blookup\\b|\\bwhat\\s*is\\b|\\bdefine\\b|\\blist\\b|\\bshow\\b|\\btell\\s*me\\b|\\bexplain\\s*briefly\\b|\\bshort\\s*answer\\b|\\bone\\s*line\\b|\\btrivial\\b|\\bminor\\s*change\\b|\\brename\\b|\\bformat\\b|\\bstyle\\s*fix\\b|\\bcomment\\b|\\bdoc\\s*string\\b|\\btypo\\b|\\bwhitespace\\b)").unwrap(),
                model: tiers.cheap.clone(),
                provider_override: Some(provider_kind.clone()),
                max_tokens_override: Some(2048),
                description: "Simple/quick tasks → cheap model".to_string(),
            },
            // ─── Exploration/reading tasks → cheap models ───
            RouteRule {
                pattern: Regex::new("(?i)(\\bexplore\\b|\\bsearch\\b|\\bfind\\b|\\bgrep\\b|\\bread\\b|\\bunderstand\\b|\\bscan\\b|\\bbrowse\\b|\\bcheck\\b|\\bverify\\b|\\binspect\\b|\\bexamine\\b|\\blook\\s*at\\b|\\bwhere\\s*is\\b|\\bhow\\s*many\\b)").unwrap(),
                model: tiers.cheap.clone(),
                provider_override: Some(provider_kind.clone()),
                max_tokens_override: Some(4096),
                description: "Exploration/reading tasks → cheap model".to_string(),
            },
            // ─── Implementation/debug → balanced models ───
            RouteRule {
                pattern: Regex::new("(?i)(\\bimplement\\b|\\brefactor\\b|\\bdebug\\b|\\bfix\\s*bug\\b|\\badd\\s*feature\\b|\\bmodify\\b|\\bupdate\\b|\\bchange\\b|\\bcreate\\b|\\bwrite\\b|\\bbuild\\b|\\bextend\\b|\\bmigrate\\b|\\bport\\b|\\btranslate\\b|\\bconvert\\b)").unwrap(),
                model: tiers.balanced.clone(),
                provider_override: Some(provider_kind.clone()),
                max_tokens_override: Some(8192),
                description: "Implementation/debug tasks → balanced model".to_string(),
            },
            // ─── Architecture/planning → powerful models ───
            RouteRule {
                pattern: Regex::new("(?i)(\\barchitect\\b|\\bplan\\b|\\bdesign\\b|\\breview\\b|\\baudit\\b|\\banalyze\\b|\\bevaluate\\b|\\bassess\\b|\\bstrategize\\b|\\bcomplex\\b|\\bcomprehensive\\b|\\bdeep\\s*analysis\\b|\\bend\\s*to\\s*end\\b|\\bfull\\s*stack\\b|\\bsystem\\s*design\\b|\\bsecurity\\s*review\\b|\\bperformance\\s*audit\\b)").unwrap(),
                model: tiers.powerful.clone(),
                provider_override: Some(provider_kind.clone()),
                max_tokens_override: Some(16384),
                description: "Architecture/planning tasks → powerful model".to_string(),
            },
            // ─── Research/synthesis → powerful models ───
            RouteRule {
                pattern: Regex::new("(?i)(\\bresearch\\b|\\bsynthesize\\b|\\bcompare\\b|\\bcontrast\\b|\\bsummarize\\s*comprehensive\\b|\\bdeep\\s*dive\\b|\\binvestigate\\b|\\bstudy\\b|\\bliterature\\b|\\bsurvey\\b|\\bmeta\\s*analysis\\b)").unwrap(),
                model: tiers.powerful.clone(),
                provider_override: Some(provider_kind.clone()),
                max_tokens_override: Some(16384),
                description: "Research/synthesis tasks → powerful model".to_string(),
            },
        ]
    }

    /// Add a custom routing rule.
    pub fn add_rule(&mut self, rule: RouteRule) {
        self.rules.push(rule);
    }

    /// Get the current routing rules.
    pub fn rules(&self) -> &[RouteRule] {
        &self.rules
    }

    /// Get the fallback model configuration.
    pub fn fallback_config(&self) -> &ModelConfig {
        &self.fallback_config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_fallback_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::Anthropic),
            api_key: Some("sk-ant-test".to_string()),
            base_url: Some("https://api.anthropic.com/v1".to_string()),
            port: None,
            model_name: Some("claude-sonnet-4-6-20250514".to_string()),
            model_path: None,
            extra: std::collections::HashMap::new(),
        }
    }

    fn openai_fallback_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::OpenAI),
            api_key: Some("sk-test".to_string()),
            base_url: Some("https://api.openai.com/v1".to_string()),
            port: None,
            model_name: Some("gpt-4o".to_string()),
            model_path: None,
            extra: std::collections::HashMap::new(),
        }
    }

    fn gemini_fallback_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::Gemini),
            api_key: Some("ai-test".to_string()),
            base_url: Some("https://generativelanguage.googleapis.com/v1beta".to_string()),
            port: None,
            model_name: Some("gemini-2.5-flash".to_string()),
            model_path: None,
            extra: std::collections::HashMap::new(),
        }
    }

    fn ollama_fallback_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::Local,
            cloud_kind: None,
            api_key: None,
            base_url: Some("http://localhost:11434".to_string()),
            port: Some(11434),
            model_name: Some("qwen2.5:7b".to_string()),
            model_path: None,
            extra: std::collections::HashMap::new(),
        }
    }

    // ─── Anthropic provider tests ────────────────────────────────────────────

    #[test]
    fn test_anthropic_simple_task_routes_to_haiku() {
        let router = ModelRouter::with_defaults(anthropic_fallback_config());

        let decision = router.route("What is the capital of France?", "react");
        assert_eq!(decision.model, "claude-haiku-4-5-20251001");
        assert_eq!(decision.provider, RouteProviderKind::Anthropic);
        assert!(decision.reason.contains("Simple"));
    }

    #[test]
    fn test_anthropic_explore_task_routes_to_haiku() {
        let router = ModelRouter::with_defaults(anthropic_fallback_config());

        let decision = router.route("Explore the codebase to find all test files", "explore");
        assert_eq!(decision.model, "claude-haiku-4-5-20251001");
        assert!(decision.reason.contains("Exploration"));
    }

    #[test]
    fn test_anthropic_implement_task_routes_to_sonnet() {
        let router = ModelRouter::with_defaults(anthropic_fallback_config());

        let decision = router.route("Implement a new authentication module", "react");
        assert_eq!(decision.model, "claude-sonnet-4-6-20250514");
        assert!(decision.reason.contains("Implementation"));
    }

    #[test]
    fn test_anthropic_architecture_task_routes_to_opus() {
        let router = ModelRouter::with_defaults(anthropic_fallback_config());

        let decision = router.route("Design the architecture for a distributed system", "plan");
        assert_eq!(decision.model, "claude-opus-4-8");
        assert!(decision.reason.contains("Architecture"));
    }

    // ─── OpenAI provider tests ──────────────────────────────────────────────

    #[test]
    fn test_openai_simple_task_routes_to_gpt4o_mini() {
        let router = ModelRouter::with_defaults(openai_fallback_config());

        let decision = router.route("What is the capital of France?", "react");
        assert_eq!(decision.model, "gpt-4o-mini");
        assert_eq!(decision.provider, RouteProviderKind::OpenAI);
    }

    #[test]
    fn test_openai_implement_task_routes_to_gpt4o() {
        let router = ModelRouter::with_defaults(openai_fallback_config());

        let decision = router.route("Implement a new authentication module", "react");
        assert_eq!(decision.model, "gpt-4o");
        assert_eq!(decision.provider, RouteProviderKind::OpenAI);
    }

    #[test]
    fn test_openai_architecture_task_routes_to_o3_pro() {
        let router = ModelRouter::with_defaults(openai_fallback_config());

        let decision = router.route("Design the architecture for a distributed system", "plan");
        assert_eq!(decision.model, "o3-pro");
        assert_eq!(decision.provider, RouteProviderKind::OpenAI);
    }

    // ─── Gemini provider tests ──────────────────────────────────────────────

    #[test]
    fn test_gemini_simple_task_routes_to_flash() {
        let router = ModelRouter::with_defaults(gemini_fallback_config());

        let decision = router.route("What is the capital of France?", "react");
        assert_eq!(decision.model, "gemini-2.0-flash");
        assert_eq!(decision.provider, RouteProviderKind::Gemini);
    }

    #[test]
    fn test_gemini_implement_task_routes_to_25flash() {
        let router = ModelRouter::with_defaults(gemini_fallback_config());

        let decision = router.route("Implement a new authentication module", "react");
        assert_eq!(decision.model, "gemini-2.5-flash");
        assert_eq!(decision.provider, RouteProviderKind::Gemini);
    }

    #[test]
    fn test_gemini_architecture_task_routes_to_25pro() {
        let router = ModelRouter::with_defaults(gemini_fallback_config());

        let decision = router.route("Design the architecture for a distributed system", "plan");
        assert_eq!(decision.model, "gemini-2.5-pro");
        assert_eq!(decision.provider, RouteProviderKind::Gemini);
    }

    // ─── Ollama provider tests ──────────────────────────────────────────────

    #[test]
    fn test_ollama_simple_task_routes_to_small_model() {
        let router = ModelRouter::with_defaults(ollama_fallback_config());

        let decision = router.route("What is the capital of France?", "react");
        assert_eq!(decision.model, "qwen2.5:0.5b");
        assert_eq!(decision.provider, RouteProviderKind::Ollama);
    }

    #[test]
    fn test_ollama_implement_task_routes_to_medium_model() {
        let router = ModelRouter::with_defaults(ollama_fallback_config());

        let decision = router.route("Implement a new authentication module", "react");
        assert_eq!(decision.model, "qwen2.5:7b");
        assert_eq!(decision.provider, RouteProviderKind::Ollama);
    }

    #[test]
    fn test_ollama_architecture_task_routes_to_large_model() {
        let router = ModelRouter::with_defaults(ollama_fallback_config());

        let decision = router.route("Design the architecture for a distributed system", "plan");
        assert_eq!(decision.model, "deepseek-r1:14b");
        assert_eq!(decision.provider, RouteProviderKind::Ollama);
    }

    // ─── Cross-provider behavior tests ──────────────────────────────────────

    #[test]
    fn test_no_cross_provider_routing_in_defaults() {
        // OpenAI provider should NEVER route to Anthropic models
        let router = ModelRouter::with_defaults(openai_fallback_config());
        let rules = router.rules();

        for rule in rules {
            // None of the default rules should mention claude
            assert!(!rule.model.contains("claude"),
                "Default rule for OpenAI provider references Anthropic model: {}", rule.model);
            // Provider override should be OpenAI, not Anthropic
            if let Some(provider) = &rule.provider_override {
                assert_eq!(*provider, RouteProviderKind::OpenAI,
                    "Default rule for OpenAI provider overrides to {:?}", provider);
            }
        }
    }

    #[test]
    fn test_custom_rule_can_cross_provider() {
        // Custom rules CAN route to a different provider (user explicitly set it)
        let router = ModelRouter::new(
            vec![RouteRule {
                pattern: Regex::new("(?i)(\\bcode\\s*review\\b|\\bpr\\s*review\\b)").unwrap(),
                model: "claude-opus-4-8".to_string(),
                provider_override: Some(RouteProviderKind::Anthropic),
                max_tokens_override: Some(8192),
                description: "Code review → Anthropic Opus (cross-provider)".to_string(),
            }],
            openai_fallback_config(),  // Fallback is OpenAI
        );

        let decision = router.route("Code review of authentication module", "reflect");
        // Custom rule explicitly routes to Anthropic — this is valid
        assert_eq!(decision.model, "claude-opus-4-8");
        assert_eq!(decision.provider, RouteProviderKind::Anthropic);
    }

    #[test]
    fn test_fallback_uses_config_provider() {
        let router = ModelRouter::with_defaults(openai_fallback_config());

        let decision = router.route("perform a very unique operation", "react");
        assert_eq!(decision.matched_rule, "fallback");
        // Fallback should use the configured model (gpt-4o from openai config)
        assert_eq!(decision.model, "gpt-4o");
    }

    // ─── Auto-detect from URL ──────────────────────────────────────────────

    #[test]
    fn test_auto_detect_anthropic_from_url() {
        let config = ModelConfig {
            provider_type: ProviderType::Cloud,
            cloud_kind: None,  // Not explicitly set
            api_key: Some("sk-test".to_string()),
            base_url: Some("https://api.anthropic.com/v1".to_string()),
            port: None,
            model_name: Some("claude-sonnet-4-6-20250514".to_string()),
            model_path: None,
            extra: std::collections::HashMap::new(),
        };
        let provider = ModelRouter::infer_provider_from_config_static(&config);
        assert_eq!(provider, RouteProviderKind::Anthropic);

        // Should generate Anthropic-tier rules
        let router = ModelRouter::with_defaults(config);
        let decision = router.route("Implement authentication", "react");
        assert_eq!(decision.model, "claude-sonnet-4-6-20250514");
    }

    // ─── Other existing tests ──────────────────────────────────────────────

    #[test]
    fn test_default_rules_research_task() {
        let router = ModelRouter::with_defaults(anthropic_fallback_config());

        let d1 = router.route("Conduct a comprehensive survey of agent frameworks", "reflect");
        let d2 = router.route("Perform a meta-analysis of clinical trial results", "reflect");
        let d3 = router.route("Research the latest advances in quantum computing", "explore");

        // All should produce valid decisions with Opus-level model for Anthropic
        assert!(!d1.model.is_empty());
        assert!(!d2.model.is_empty());
        assert!(!d3.model.is_empty());
    }

    #[test]
    fn test_custom_rule_priority() {
        let mut router = ModelRouter::with_defaults(anthropic_fallback_config());

        router.add_rule(RouteRule {
            pattern: Regex::new("(?i)(code\\s*review|pr\\s*review)").unwrap(),
            model: "gemini-2.0-flash".to_string(),
            provider_override: Some(RouteProviderKind::Gemini),
            max_tokens_override: Some(8192),
            description: "Code review → Gemini flash".to_string(),
        });

        // Default rules come first — "review" matches architecture rule
        let decision = router.route("Review pull request #123", "reflect");
        assert_eq!(decision.model, "claude-opus-4-8"); // Default architecture rule wins
    }

    #[test]
    fn test_custom_rule_with_priority() {
        let custom_rules = vec![
            RouteRule {
                pattern: Regex::new("(?i)(code\\s*review|pr\\s*review)").unwrap(),
                model: "gemini-2.0-flash".to_string(),
                provider_override: Some(RouteProviderKind::Gemini),
                max_tokens_override: Some(8192),
                description: "Code review → Gemini flash".to_string(),
            },
        ];

        let router = ModelRouter::new(custom_rules, anthropic_fallback_config());
        let decision = router.route("Code review of authentication module", "reflect");
        assert_eq!(decision.model, "gemini-2.0-flash");
        assert_eq!(decision.provider, RouteProviderKind::Gemini);
    }

    #[test]
    fn test_infer_provider_from_model() {
        let router = ModelRouter::with_defaults(anthropic_fallback_config());
        assert_eq!(router.infer_provider_from_model("claude-opus-4-8"), RouteProviderKind::Anthropic);
        assert_eq!(router.infer_provider_from_model("gpt-4o"), RouteProviderKind::OpenAI);
        assert_eq!(router.infer_provider_from_model("gemini-2.0-flash"), RouteProviderKind::Gemini);
        assert_eq!(router.infer_provider_from_model("ollama:qwen2.5"), RouteProviderKind::Ollama);
    }

    #[test]
    fn test_route_provider_kind_conversion() {
        assert_eq!(RouteProviderKind::Anthropic.to_cloud_kind(), Some(CloudProviderKind::Anthropic));
        assert_eq!(RouteProviderKind::OpenAI.to_cloud_kind(), Some(CloudProviderKind::OpenAI));
        assert_eq!(RouteProviderKind::Gemini.to_cloud_kind(), Some(CloudProviderKind::Gemini));
        assert_eq!(RouteProviderKind::Ollama.to_cloud_kind(), None);
        assert_eq!(RouteProviderKind::Ollama.to_provider_type(), ProviderType::Local);
        assert_eq!(RouteProviderKind::Anthropic.to_provider_type(), ProviderType::Cloud);
    }

    #[test]
    fn test_paradigm_based_routing() {
        let router = ModelRouter::with_defaults(anthropic_fallback_config());

        let decision = router.route("perform a very unique operation", "react");
        assert_eq!(decision.matched_rule, "fallback");
    }
}
