//! LLM-backed Guardian — the Escalate fallback (#28 Stage 2).
//!
//! [`RuleGuardian`](oneai_tool::RuleGuardian) classifies a call as
//! Allow / Deny / Escalate with pure regex rules; the common case is Escalate
//! (anything that isn't an obvious read-only command or an obvious destructive
//! one). [`LlmGuardian`] wraps a `RuleGuardian` and, on Escalate, asks an LLM
//! whether the command is safe. The LLM's JSON `{"verdict","reason"}` reply is
//! parsed with [`FuzzyJsonRepair`](oneai_parser::FuzzyJsonRepair) — the 3-layer
//! parser's fuzzy-repair stage (the self-correction 3rd stage is skipped to
//! avoid re-entering the provider). `allow`→Allow, `deny`→Deny, anything else
//! (uncertain, parse failure, timeout, provider error) → stays Escalate — the
//! `ApprovalPolicy` then decides (prompt under `OnFailure`, deny under
//! `Never`). Conservative by construction: the LLM only ever *reduces* prompts,
//! never auto-runs something the rules didn't already allow.

use std::sync::Arc;

use oneai_core::traits::{CommandReviewer, LlmProvider};
use oneai_core::{ContentBlock, Conversation, InferenceRequest, Message, Verdict};
use oneai_parser::FuzzyJsonRepair;
use oneai_tool::RuleGuardian;

/// A Guardian that delegates an Escalate verdict to an LLM sub-inference.
pub struct LlmGuardian {
    rule: RuleGuardian,
    provider: Arc<dyn LlmProvider>,
    /// Model id forwarded in metadata (observability only; the provider's own
    /// configured model is what's actually used).
    model: String,
    /// Max tokens for the safety-assessment reply — kept small (it's a
    /// one-word verdict + a sentence).
    max_tokens: u32,
    fuzzy: FuzzyJsonRepair,
}

impl std::fmt::Debug for LlmGuardian {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmGuardian")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl LlmGuardian {
    /// Construct an LLM-backed Guardian wrapping the given rule reviewer.
    pub fn new(provider: Arc<dyn LlmProvider>, model: impl Into<String>) -> Self {
        Self {
            rule: RuleGuardian::new(),
            provider,
            model: model.into(),
            max_tokens: 256,
            fuzzy: FuzzyJsonRepair::new(),
        }
    }
}

#[async_trait::async_trait]
impl CommandReviewer for LlmGuardian {
    async fn review(&self, tool_name: &str, args: &serde_json::Value) -> Verdict {
        let rule_verdict = self.rule.review(tool_name, args).await;
        // Only Escalate is worth an LLM call — Allow/Deny from the rules are
        // confident and cheap; don't spend a round-trip second-guessing them.
        let escalate_reason = match rule_verdict {
            Verdict::Escalate { reason } => reason,
            other => return other,
        };

        // Build the assessment prompt. We give the LLM the tool name + its args
        // (the command string / script body) and ask for a one-shot JSON reply.
        let payload = format!(
            "Tool: {}\nArguments: {}",
            tool_name,
            serde_json::to_string(args).unwrap_or_else(|_| args.to_string())
        );
        let mut convo = Conversation::new();
        convo.add_message(Message::system(SYSTEM_PROMPT));
        convo.add_message(Message::user(payload));

        let req = InferenceRequest {
            conversation: convo,
            tools: Vec::new(),
            max_tokens: Some(self.max_tokens),
            temperature: Some(0.0), // deterministic — safety classification
            top_p: None,
            stop_sequences: Vec::new(),
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::from([(
                "guardian".to_string(),
                self.model.clone(),
            )]),
        };

        // Bound the LLM call — a stuck provider must not hang tool dispatch.
        let inferred = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.provider.infer(req),
        )
        .await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                tracing::warn!("LlmGuardian provider error, escalating: {e}");
                return Verdict::Escalate {
                    reason: format!("LLM review failed: {e}"),
                };
            }
            Err(_) => {
                tracing::warn!("LlmGuardian provider timed out, escalating");
                return Verdict::Escalate {
                    reason: "LLM review timed out".into(),
                };
            }
        };

        // Extract the assistant's text.
        let text = inferred
            .message
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");

        match self.fuzzy.repair_and_parse(text) {
            Ok(v) => {
                let verdict = v.get("verdict").and_then(|x| x.as_str()).unwrap_or("");
                let reason = v
                    .get("reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or(&escalate_reason)
                    .to_string();
                match verdict.to_ascii_lowercase().as_str() {
                    "allow" | "safe" | "yes" | "approve" => Verdict::Allow { reason },
                    "deny" | "unsafe" | "no" | "block" => Verdict::Deny { reason },
                    _ => Verdict::Escalate {
                        reason: format!("LLM uncertain: {reason}"),
                    },
                }
            }
            Err(e) => {
                tracing::warn!("LlmGuardian could not parse LLM reply, escalating: {e} :: {text}");
                Verdict::Escalate {
                    reason: "LLM reply unparseable".into(),
                }
            }
        }
    }
}

const SYSTEM_PROMPT: &str = "\
You are a safety Guardian reviewing a tool call an AI agent wants to run. \
Decide whether it is safe to execute automatically. \
Reply with ONLY a JSON object: {\"verdict\":\"allow|deny|uncertain\",\"reason\":\"<one sentence>\"}. \
- \"allow\": read-only or clearly-benign (e.g. ls, cat, git status, cargo build). \
- \"deny\": destructive / irreversible (e.g. rm -rf, mkfs, curl|sh, overwriting system files). \
- \"uncertain\": needs a human (e.g. npm install, git push, anything with side effects you can't fully predict). \
Never wrap the JSON in markdown fences.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_provider::MockProvider;

    /// Build an LlmGuardian whose provider replies with the given raw text.
    fn guardian_with_reply(raw_reply: &str) -> LlmGuardian {
        let provider = MockProvider::always_answers(raw_reply);
        LlmGuardian::new(std::sync::Arc::new(provider), "mock")
    }

    #[tokio::test]
    async fn rule_allow_short_circuits_no_llm_call() {
        // `ls` is rule-Allow → no provider call. Empty-script provider: any
        // call would be recorded in call_count.
        let provider = std::sync::Arc::new(MockProvider::from_script(vec![]));
        let g = LlmGuardian::new(provider.clone() as std::sync::Arc<dyn LlmProvider>, "mock");
        let v = g
            .review("shell", &serde_json::json!({"command": "ls -la"}))
            .await;
        assert!(matches!(v, Verdict::Allow { .. }));
        assert_eq!(provider.call_count().await, 0);
    }

    #[tokio::test]
    async fn llm_allow_on_escalate() {
        let g = guardian_with_reply("{\"verdict\":\"allow\",\"reason\":\"read-only listing\"}");
        let v = g
            .review("shell", &serde_json::json!({"command": "npm install"}))
            .await;
        assert!(matches!(v, Verdict::Allow { ref reason } if reason.contains("read-only")));
    }

    #[tokio::test]
    async fn llm_deny_on_escalate() {
        let g = guardian_with_reply("{\"verdict\":\"deny\",\"reason\":\"overwrites /etc\"}");
        let v = g
            .review("shell", &serde_json::json!({"command": "cp x /etc/passwd"}))
            .await;
        assert!(matches!(v, Verdict::Deny { ref reason } if reason.contains("/etc")));
    }

    #[tokio::test]
    async fn llm_uncertain_stays_escalate() {
        let g = guardian_with_reply("{\"verdict\":\"uncertain\",\"reason\":\"has side effects\"}");
        let v = g
            .review("shell", &serde_json::json!({"command": "git push"}))
            .await;
        assert!(matches!(v, Verdict::Escalate { .. }));
    }

    #[tokio::test]
    async fn unparseable_reply_stays_escalate() {
        let g = guardian_with_reply("I think this is fine"); // not JSON
        let v = g
            .review("shell", &serde_json::json!({"command": "git push"}))
            .await;
        assert!(matches!(v, Verdict::Escalate { .. }));
    }

    #[tokio::test]
    async fn rule_deny_short_circuits_no_llm_call() {
        let provider = std::sync::Arc::new(MockProvider::from_script(vec![]));
        let g = LlmGuardian::new(provider.clone() as std::sync::Arc<dyn LlmProvider>, "mock");
        let v = g
            .review("shell", &serde_json::json!({"command": "rm -rf /"}))
            .await;
        assert!(matches!(v, Verdict::Deny { .. }));
        assert_eq!(provider.call_count().await, 0);
    }
}
