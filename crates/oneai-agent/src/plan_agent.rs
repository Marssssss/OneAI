//! Plan Agent — task decomposition paradigm.
//!
//! The PlanAgent decomposes complex user tasks into ordered steps.
//! Each step is annotated with whether it is coupled (depends on previous step's output)
//! or non-coupled (can be executed independently by parallel sub-agents).
//!
//! Output format: a list of PlanStep items that feed into ParallelExecutor or ReActAgent.

use std::collections::HashMap;
use std::sync::Arc;

// `PlanStep` / `PlanStepStatus` now live in `oneai-core` so that
// `InteractionRequest::PlanReview` (part of the `InteractionGate` trait in
// core) can reference them without a layering violation. Re-export here for
// backward-compatible `oneai_agent::PlanStep` / `crate::plan_agent::PlanStep`
// access.
pub use oneai_core::{PlanStep, PlanStepStatus};

use oneai_core::{
    Conversation, InferenceRequest, Message, Role,
};
use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;

/// A single step in a plan.
///
/// # Deprecated location
///
/// This definition has moved to [`oneai_core::PlanStep`]. The alias above
/// re-exports it from this module for compatibility. New code should import
/// from `oneai_core` directly.

/// Result of a PlanAgent execution.
#[derive(Debug, Clone)]
pub struct PlanResult {
    /// The decomposed plan steps.
    pub steps: Vec<PlanStep>,

    /// The conversation after planning.
    pub conversation: Conversation,

    /// The raw model response.
    pub raw_response: String,
}

/// A planning tradeoff with no clearly superior option, surfaced for the user
/// to resolve before the final plan is produced (stage 1 of two-stage planning).
#[derive(Debug, Clone)]
pub struct PlanDecisionRequest {
    /// Stable id, e.g. `"d1"`.
    pub decision_id: String,
    /// The question requiring a decision.
    pub question: String,
    /// Why the user's input is needed.
    pub context: String,
    /// The selectable options.
    pub options: Vec<oneai_core::DecisionOption>,
}

/// The user's resolution to a [`PlanDecisionRequest`], fed into stage 2.
#[derive(Debug, Clone)]
pub struct PlanDecisionResolution {
    /// Which decision this resolves (matches `PlanDecisionRequest::decision_id`).
    pub decision_id: String,
    /// The id of the chosen option, if the user picked one.
    pub chosen: String,
    /// Free-text custom guidance, if the user supplied their own answer
    /// (Revise) instead of picking an option.
    pub custom: Option<String>,
}

/// Configuration for a Plan agent.
#[derive(Debug, Clone)]
pub struct PlanConfig {
    /// System prompt for the planning agent.
    pub system_prompt: String,

    /// Temperature for planning inference (lower = more deterministic).
    pub temperature: Option<f32>,

    /// Maximum tokens for the planning response.
    pub max_tokens: Option<u32>,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            system_prompt: PLAN_SYSTEM_PROMPT.to_string(),
            temperature: Some(0.0), // Deterministic planning
            max_tokens: Some(2048),
        }
    }
}

/// Default system prompt for the planning agent (two-stage).
const PLAN_SYSTEM_PROMPT: &str = "\
You are a task planning assistant. You plan in TWO stages.

STAGE 1 — scan for decisions that involve a genuine tradeoff with NO clearly
superior option (e.g. speed vs correctness, cost vs quality, library A vs B,
breadth vs depth). Output stage 1 as JSON:
```json
{\"decisions\": [
  {\"decision_id\": \"d1\", \"question\": \"优先速度还是正确性？\", \"context\": \"...\",
   \"options\": [
     {\"id\": \"opt_a\", \"label\": \"优先速度\", \"description\": \"...\", \"tradeoffs\": \"...\"},
     {\"id\": \"opt_b\", \"label\": \"优先正确性\", \"description\": \"...\", \"tradeoffs\": \"...\"}
   ]}
]}
```
Rules for decisions:
- Only surface a decision when the options are genuinely comparable (no clear
  winner). If one option is objectively better for this task, pick it silently
  — do NOT ask.
- Keep decisions to the minimum necessary (typically 0–3). Trivial choices are
  not decisions.
- For each decision, provide 2–4 options, each with its tradeoff stated honestly.
- If there are no decisions, output exactly: {\"decisions\": []}

STAGE 2 — after you receive the user's choices (or an empty list if none were
asked), output the SINGLE final plan as a JSON array with this exact format
(do NOT produce multiple final plans):
```json
[
  {\"id\": \"step_1\", \"description\": \"Brief description of what to do\",
   \"coupled\": false, \"depends_on\": []},
  {\"id\": \"step_2\", \"description\": \"Brief description\", \"coupled\": true,
   \"depends_on\": [\"step_1\"]}
]
```
Stage 2 rules:
1. Start IDs from step_1 and increment sequentially.
2. coupled=true means the step depends on a previous step's output; list those
   IDs in depends_on. Non-coupled steps have empty depends_on.
3. Keep descriptions concise but actionable.
4. Output ONLY the JSON, no other text.";

/// Plan Agent — decomposes complex tasks into ordered steps.
pub struct PlanAgent {
    /// The LLM provider.
    provider: Arc<dyn LlmProvider>,

    /// Configuration.
    config: PlanConfig,
}

impl PlanAgent {
    /// Create a new Plan agent.
    pub fn new(provider: Arc<dyn LlmProvider>, config: PlanConfig) -> Self {
        Self { provider, config }
    }

    /// Create with default configuration.
    pub fn with_defaults(provider: Arc<dyn LlmProvider>) -> Self {
        Self::new(provider, PlanConfig::default())
    }

    /// Decompose a task into plan steps.
    ///
    /// # Deprecated
    ///
    /// This skips stage-1 decision identification. Prefer
    /// [`identify_decisions`](Self::identify_decisions) +
    /// [`plan_with_resolutions`](Self::plan_with_resolutions), or call
    /// [`plan_with_resolutions`](Self::plan_with_resolutions) directly with an
    /// empty resolution list for the old one-shot behavior.
    #[deprecated(since = "0.2.0", note = "use plan_with_resolutions / identify_decisions instead")]
    pub async fn plan(&self, task: &str) -> Result<PlanResult> {
        self.plan_with_resolutions(task, &[]).await
    }

    /// Stage 1: scan the task for tradeoffs that need the user's input.
    ///
    /// Returns the decisions the planner surfaced (may be empty). The caller
    /// resolves each via `InteractionGate::request(PlanDecision{..})` and feeds
    /// the results into [`plan_with_resolutions`](Self::plan_with_resolutions).
    pub async fn identify_decisions(&self, task: &str) -> Result<Vec<PlanDecisionRequest>> {
        let mut conv = Conversation::new();
        conv.add_message(Message::system(self.config.system_prompt.clone()));
        conv.add_message(Message::user(format!(
            "Stage 1: scan this task for decisions with no clearly superior option. \
             Output only the JSON {{\"decisions\":[...]}} per the prompt rules.\n\nTask:\n{}",
            task
        )));

        let raw = self.infer_text(&mut conv).await?;
        Ok(crate::plan_state::parse_decisions(&raw))
    }

    /// Stage 2: produce the SINGLE final plan, given the user's resolutions.
    ///
    /// `resolutions` is the list of user answers to the decisions surfaced by
    /// [`identify_decisions`](Self::identify_decisions) (empty if there were
    /// none). Each resolution is injected into the user message as guidance so
    /// the model bakes the decisions into the final plan.
    pub async fn plan_with_resolutions(
        &self,
        task: &str,
        resolutions: &[PlanDecisionResolution],
    ) -> Result<PlanResult> {
        let mut conv = Conversation::new();
        conv.add_message(Message::system(self.config.system_prompt.clone()));

        let mut user_msg = format!(
            "Stage 2: produce the SINGLE final plan as a JSON array of steps per the \
             prompt rules. Do not produce multiple plans. Output ONLY the JSON.\n\nTask:\n{}\n",
            task
        );
        if resolutions.is_empty() {
            user_msg.push_str("\n(No user decisions were needed for this task.)\n");
        } else {
            user_msg.push_str("\nUser decisions to bake into the plan:\n");
            for r in resolutions {
                if let Some(custom) = &r.custom {
                    user_msg.push_str(&format!("- {}: {}\n", r.decision_id, custom));
                } else {
                    user_msg.push_str(&format!("- {}: chose {}\n", r.decision_id, r.chosen));
                }
            }
        }
        conv.add_message(Message::user(user_msg));

        let raw_response = self.infer_text(&mut conv).await?;
        let steps = parse_plan_steps(&raw_response)?;
        Ok(PlanResult {
            steps,
            conversation: conv,
            raw_response,
        })
    }

    /// Run a single no-tools inference against `conv`, append the assistant
    /// message, and return its text content.
    async fn infer_text(&self, conv: &mut Conversation) -> Result<String> {
        let request = InferenceRequest {
            conversation: conv.clone(),
            tools: vec![],
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };
        let response = self.provider.infer(request).await?;
        let text = response.message.text_content();
        conv.add_message(response.message);
        Ok(text)
    }

    /// Decompose a task within an existing conversation context.
    pub async fn plan_in_context(&self, conversation: &Conversation, task: &str) -> Result<PlanResult> {
        let mut conv = conversation.clone();
        if !conv.messages.iter().any(|m| m.role == Role::System) {
            conv.add_message(Message::system(self.config.system_prompt.clone()));
        }
        conv.add_message(Message::user(format!(
            "Please decompose this task into steps:\n\n{}", task
        )));

        let request = InferenceRequest {
            conversation: conv.clone(),
            tools: vec![],
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        };

        let response = self.provider.infer(request).await?;
        let raw_response = response.message.text_content();
        conv.add_message(response.message.clone());

        let steps = parse_plan_steps(&raw_response)?;

        Ok(PlanResult {
            steps,
            conversation: conv,
            raw_response,
        })
    }
}

/// Parse plan steps from the model's response.
///
/// Attempts to extract a JSON array from the response text,
/// using the 3-layer parsing defense (Layer 2: fuzzy repair).
pub fn parse_plan_steps(raw: &str) -> Result<Vec<PlanStep>> {
    // Try direct JSON parse first
    if let Ok(steps) = serde_json::from_str::<Vec<PlanStep>>(raw) {
        return Ok(steps);
    }

    // Try extracting JSON from the response (Layer 2: fuzzy repair)
    // Look for JSON array markers
    let start = raw.find('[');
    let end = raw.rfind(']');

    if let (Some(s), Some(e)) = (start, end) {
        let json_fragment = &raw[s..e + 1];

        if let Ok(steps) = serde_json::from_str::<Vec<PlanStep>>(json_fragment) {
            return Ok(steps);
        }

        // Try closing unclosed brackets
        let mut repaired = json_fragment.to_string();
        let open_brackets = repaired.chars().filter(|c| *c == '[').count();
        let close_brackets = repaired.chars().filter(|c| *c == ']').count();
        for _ in 0..(open_brackets - close_brackets) {
            repaired.push(']');
        }

        if let Ok(steps) = serde_json::from_str::<Vec<PlanStep>>(repaired.as_str()) {
            return Ok(steps);
        }
    }

    // If parsing fails completely, create a single-step plan
    // (fallback: treat the entire task as one ReAct step)
    Ok(vec![PlanStep {
        id: "step_1".to_string(),
        description: raw.to_string(),
        coupled: false,
        depends_on: vec![],
        status: PlanStepStatus::default(),
        active_form: None,
    }])
}