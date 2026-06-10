//! Plan Agent — task decomposition paradigm.
//!
//! The PlanAgent decomposes complex user tasks into ordered steps.
//! Each step is annotated with whether it is coupled (depends on previous step's output)
//! or non-coupled (can be executed independently by parallel sub-agents).
//!
//! Output format: a list of PlanStep items that feed into ParallelExecutor or ReActAgent.

use std::collections::HashMap;
use std::sync::Arc;

use oneai_core::{
    Conversation, InferenceRequest, Message, Role,
};
use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;

/// A single step in a plan.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PlanStep {
    /// Step identifier (e.g., "step_1").
    pub id: String,

    /// Brief description of what this step should accomplish.
    pub description: String,

    /// Whether this step is coupled to previous steps.
    /// - Coupled: must wait for previous step's result → ReAct pipeline
    /// - Non-coupled: can run independently → parallel sub-agent
    pub coupled: bool,

    /// Which previous step IDs this step depends on (if coupled).
    #[serde(default)]
    pub depends_on: Vec<String>,
}

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

/// Default system prompt for the planning agent.
const PLAN_SYSTEM_PROMPT: &str = "\
You are a task planning assistant. When given a complex task, decompose it into clear, ordered steps.

For each step, determine whether it is:
- COUPLED: This step depends on the output of a previous step and must wait for it.
- NON-COUpled: This step can be executed independently in parallel with other non-coupled steps.

Output your plan as a JSON array with this exact format:
```json
[
  {
    \"id\": \"step_1\",
    \"description\": \"Brief description of what to do\",
    \"coupled\": false,
    \"depends_on\": []
  },
  {
    \"id\": \"step_2\",
    \"description\": \"Brief description\",
    \"coupled\": true,
    \"depends_on\": [\"step_1\"]
  }
]
```

Important rules:
1. Start IDs from step_1 and increment sequentially.
2. If a step is coupled, list all step IDs it depends on in depends_on.
3. Non-coupled steps should have empty depends_on arrays.
4. Keep descriptions concise but actionable.
5. Output ONLY the JSON array, no other text.";

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
    /// Takes a user's task description and returns a list of PlanSteps.
    pub async fn plan(&self, task: &str) -> Result<PlanResult> {
        let mut conv = Conversation::new();
        conv.add_message(Message::system(self.config.system_prompt.clone()));
        conv.add_message(Message::user(format!(
            "Please decompose this task into steps:\n\n{}", task
        )));

        let request = InferenceRequest {
            conversation: conv.clone(),
            tools: vec![], // Planning doesn't need tools
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            metadata: HashMap::new(),
        };

        let response = self.provider.infer(request).await?;

        // Parse the plan from the response
        let raw_response = response.message.text_content();
        conv.add_message(response.message.clone());

        let steps = parse_plan_steps(&raw_response)?;

        Ok(PlanResult {
            steps,
            conversation: conv,
            raw_response,
        })
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
    }])
}