//! Inference snapshot — the concrete API request + response for one iteration
//! (issue #40 trajectory panel follow-up).
//!
//! Where [`crate::ContextSnapshot`] shows *what the model saw* (the sectioned
//! context), this shows *what was actually sent and returned*: the request
//! parameters (model / sampling knobs), the tool set, the raw request
//! conversation, the model's response message, its token usage, and the
//! wall-clock inference latency. A trajectory UI can render the inference node's
//! "API request/response + metrics" detail straight off this.

use serde::{Deserialize, Serialize};

use crate::types::{InferenceResponse, Message};

/// A snapshot of one inference call (request + response + timing).
///
/// `request_messages` is the raw conversation handed to the provider (the same
/// assembly `ContextSnapshot` sections, but in wire order). The producer trims
/// each message's long text blocks so the snapshot stays bounded — a huge tool
/// result or base prompt is capped with a `[...truncated]` marker rather than
/// ballooning the trajectory event log. `response` is the model's full reply
/// (message + usage + model), which is normally small (a single assistant turn).
// NOTE: deliberately NOT `#[non_exhaustive]` — constructed cross-crate by the
// agent loop (struct expressions can't cross the non-exhaustive boundary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceSnapshot {
    /// The iteration this inference belongs to.
    pub iteration: usize,
    /// Model name the provider resolved for this request.
    pub model: String,
    /// Sampling temperature (None ⇒ provider/scenario default).
    pub temperature: Option<f32>,
    /// Maximum output tokens (None ⇒ provider default).
    pub max_tokens: Option<u32>,
    /// Nucleus-sampling top-p (None ⇒ provider default).
    pub top_p: Option<f32>,
    /// Extended-thinking token budget (None ⇒ disabled).
    pub thinking_budget: Option<u32>,
    /// Names of the tool definitions sent with the request.
    pub tool_names: Vec<String>,
    /// Number of messages in the request conversation.
    pub message_count: usize,
    /// The request conversation (text blocks trimmed) handed to the provider.
    pub request_messages: Vec<Message>,
    /// The model's response message + usage + model.
    pub response: InferenceResponse,
    /// Wall-clock inference latency in milliseconds.
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inference_snapshot_roundtrips() {
        let snap = InferenceSnapshot {
            iteration: 2,
            model: "gpt-4o".to_string(),
            temperature: Some(0.3),
            max_tokens: Some(4096),
            top_p: None,
            thinking_budget: None,
            tool_names: vec!["shell".to_string()],
            message_count: 1,
            request_messages: vec![Message::user("hi")],
            response: InferenceResponse {
                message: Message::assistant("hello"),
                usage: crate::types::TokenUsage::new(10, 4),
                model: "gpt-4o".to_string(),
                metadata: std::collections::HashMap::new(),
            },
            duration_ms: 1234,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: InferenceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.iteration, 2);
        assert_eq!(back.model, "gpt-4o");
        assert_eq!(back.duration_ms, 1234);
        assert_eq!(back.response.usage.prompt_tokens, 10);
        assert_eq!(back.request_messages.len(), 1);
    }
}
