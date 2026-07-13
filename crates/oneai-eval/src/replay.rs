//! Trajectory record + replay harness ("ghost replay") — Loop Engineering's
//! deterministic-oracle primitive.
//!
//! A *trajectory* is a recorded agent run: the user input plus the ordered
//! sequence of `InferenceResponse`s the provider returned. [`ReplayProvider`]
//! re-emits those responses in order, so the AgentLoop can be re-run with NO
//! live LLM and the model's decisions are frozen. [`TrajectoryReplayer`] then
//! asserts the replayed run makes the same tool-call sequence + iteration
//! count — a regression oracle. This is the foundation for "loop tests":
//! record once, replay on every build, fail if the loop's behavior drifted.
//!
//! Recording: [`RecordingProvider`] wraps a real provider, forwards every
//! `infer()` call, and snapshots the responses into a [`Trajectory`]. Wire it
//! via `EvalRunner` / the CLI `--record <path>` flag to capture a trajectory
//! from a real run, then `eval replay <path>` to replay it deterministically.
//!
//! A/B replay (e.g. `PromptCachePolicy::On` vs `Off`) needs a *live* provider
//! (frozen responses carry the original cache stats), so it lives in the
//! real-LLM validation step rather than here.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::LlmProvider;
use oneai_core::{
    InferenceRequest, InferenceResponse, InferenceStreamChunk, ModelCapability,
    ModelConfig,
};
use oneai_trace::SpanKind;

/// A recorded agent run: the input plus the ordered provider responses and a
/// digest of the tool-call sequence / iteration count for the determinism check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trajectory {
    /// The user input that drove the original run.
    pub input: String,
    /// The ordered provider responses, replayed in order by [`ReplayProvider`].
    pub responses: Vec<InferenceResponse>,
    /// Tool names called, in order, during the original run (from the trace).
    #[serde(default)]
    pub recorded_tool_calls: Vec<String>,
    /// ReAct iterations taken by the original run.
    #[serde(default)]
    pub recorded_iterations: usize,
}

impl Trajectory {
    /// Load a trajectory from a JSON file.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let data = std::fs::read(path)
            .map_err(|e| OneAIError::Config(format!("read trajectory {}: {}", path.display(), e)))?;
        serde_json::from_slice::<Trajectory>(&data)
            .map_err(|e| OneAIError::Config(format!("parse trajectory: {}", e)))
    }

    /// Save a trajectory to a JSON file.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| OneAIError::Config(format!("serialize trajectory: {}", e)))?;
        std::fs::write(path, bytes)
            .map_err(|e| OneAIError::Config(format!("write trajectory: {}", e)))
    }
}

/// A provider that replays a recorded [`Trajectory`]'s responses in call order.
/// Used by [`TrajectoryReplayer`] to re-run the AgentLoop deterministically
/// with no live LLM. Calls past the end of the recording return an error
/// (the loop drifted — made more inference calls than originally).
pub struct ReplayProvider {
    responses: Vec<InferenceResponse>,
    index: AtomicUsize,
    config: ModelConfig,
}

impl ReplayProvider {
    /// Build a replay provider from a recorded response sequence. The
    /// `ModelConfig` is returned by `config()`; pass the original model's
    /// config if available, else `ModelConfig::default()`.
    pub fn new(responses: Vec<InferenceResponse>, config: ModelConfig) -> Self {
        Self { responses, config, index: AtomicUsize::new(0) }
    }

    /// Build a replay provider from a loaded trajectory.
    pub fn from_trajectory(t: &Trajectory) -> Self {
        Self::new(t.responses.clone(), ModelConfig::default())
    }
}

#[async_trait]
impl LlmProvider for ReplayProvider {
    async fn infer(&self, _req: InferenceRequest) -> Result<InferenceResponse> {
        let i = self.index.fetch_add(1, Ordering::SeqCst);
        self.responses.get(i).cloned().ok_or_else(|| OneAIError::Provider(format!(
            "ReplayProvider exhausted: recording had {} responses, call #{} requested. \
             The loop made more inference calls than the recorded trajectory — behavior drift.",
            self.responses.len(),
            i + 1,
        )))
    }

    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError> {
        // Reuse infer() and emit the whole response as one final chunk.
        let resp = self.infer(req).await?;
        let chunk = InferenceStreamChunk {
            content: resp.message.content.clone(),
            is_final: true,
            usage: Some(resp.usage.clone()),
            model: Some(resp.model.clone()),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx.send(chunk).await;
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn capabilities(&self) -> ModelCapability {
        ModelCapability::claude_class()
    }

    fn config(&self) -> &ModelConfig {
        &self.config
    }
}

// Pin is used in the infer_stream return type (re-exported above).

/// A provider wrapper that records every `infer()` response into a growable
/// buffer, so a real run can be captured into a [`Trajectory`]. The wrapped
/// provider drives the actual inference. Use it via the CLI `eval run --record
/// <path>` flag to produce a trajectory, then `eval replay <path>` to replay.
pub struct RecordingProvider {
    inner: Arc<dyn LlmProvider>,
    recorded: Mutex<Vec<InferenceResponse>>,
}

impl RecordingProvider {
    pub fn new(inner: Arc<dyn LlmProvider>) -> Self {
        Self { inner, recorded: Mutex::new(Vec::new()) }
    }

    /// Snapshot the recorded responses so far into a [`Trajectory`] (used by
    /// the CLI `--record` flag, which shares the recorder as the App's
    /// `Arc<dyn LlmProvider>` and so can't consume it).
    pub async fn trajectory(&self, input: &str, tool_calls: Vec<String>, iterations: usize) -> Trajectory {
        let responses = self.recorded.lock().await.clone();
        Trajectory {
            input: input.to_string(),
            responses,
            recorded_tool_calls: tool_calls,
            recorded_iterations: iterations,
        }
    }
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse> {
        let resp = self.inner.infer(req).await?;
        self.recorded.lock().await.push(resp.clone());
        Ok(resp)
    }

    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>, OneAIError> {
        // The agent loop uses streaming (session.rs sets use_streaming=true),
        // so infer() above wouldn't be hit. Route streaming through infer()
        // so the response is recorded, then emit it as a single final chunk.
        // Recording is a capture mode — non-streaming emission is acceptable;
        // the loop's incremental parser handles a complete-chunk-as-one-delta.
        let resp = self.infer(req).await?;
        let chunk = InferenceStreamChunk {
            content: resp.message.content.clone(),
            is_final: true,
            usage: Some(resp.usage.clone()),
            model: Some(resp.model.clone()),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx.send(chunk).await;
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn capabilities(&self) -> ModelCapability {
        self.inner.capabilities()
    }

    fn config(&self) -> &ModelConfig {
        self.inner.config()
    }
}

/// Result of replaying a trajectory — the determinism verdict + the fresh
/// efficiency profile from the replayed run.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayResult {
    /// Whether the replayed run made the same tool calls in the same order
    /// AND stayed within the recorded iteration count.
    pub deterministic: bool,
    pub replayed_tool_calls: Vec<String>,
    pub recorded_tool_calls: Vec<String>,
    pub replayed_iterations: usize,
    pub recorded_iterations: usize,
    pub efficiency: Option<crate::efficiency::EfficiencyProfile>,
}

impl ReplayResult {
    /// Whether tool-call order matched the recording exactly.
    pub fn tool_calls_match(&self) -> bool {
        self.replayed_tool_calls == self.recorded_tool_calls
    }
}

/// Re-run a recorded trajectory against a frozen [`ReplayProvider`] and check
/// determinism: the loop should make the same tool calls in the same order
/// and not exceed the recorded iteration count.
///
/// Builds its own throwaway `App` (no domain pack — replay is provider-frozen,
/// tool calls are only exercised when tools are registered; for trajectories
/// that ended in a direct answer this is a pure decision-replay).
pub async fn replay_trajectory(path: &std::path::Path) -> Result<ReplayResult> {
    let trajectory = Trajectory::load(path)?;
    replay_trajectory_with(trajectory).await
}

/// Replay a loaded trajectory and return the determinism verdict + fresh
/// efficiency profile.
pub async fn replay_trajectory_with(trajectory: Trajectory) -> Result<ReplayResult> {
    let provider = Arc::new(ReplayProvider::from_trajectory(&trajectory));
    let recorded_tool_calls = trajectory.recorded_tool_calls.clone();
    let recorded_iterations = trajectory.recorded_iterations;
    let input = trajectory.input.clone();

    let app = oneai_app::AppBuilder::new()
        .provider(provider)
        .noop_interaction_gate()
        .default_parser()
        .trace_in_memory()
        .default_usage_tracker()
        .default_token_counter()
        .build()
        .await
        .map_err(|e| OneAIError::Config(format!("replay app build: {}", e)))?;

    let mut session = app.create_session();
    let _ = session.run_agent_silent(&input).await;

    // Extract the replayed tool-call sequence + iteration count from the trace.
    let (replayed_tool_calls, replayed_iterations) = if let Some(ctx) = session.trace_context() {
        let tree = ctx.build_tree();
        let calls: Vec<String> = tree.root_span
            .spans_by_kind(SpanKind::TOOL)
            .iter()
            .filter_map(|s| s.attributes.get("tool.name"))
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let iters = oneai_trace::TraceMetrics::compute_from_tree(&tree.root_span)
            .avg_iterations.round() as usize;
        (calls, iters)
    } else {
        (Vec::new(), 0)
    };

    let efficiency = session.trace_context().map(|ctx| {
        let tree = ctx.build_tree();
        crate::efficiency::EfficiencyProfile::from_tree(
            &tree.root_span,
            0, // wall-clock not meaningful for a frozen replay (instant responses)
            0, 0, 0,
            replayed_iterations,
        )
    });

    let deterministic = replayed_tool_calls == recorded_tool_calls
        && (recorded_iterations == 0 || replayed_iterations <= recorded_iterations);

    Ok(ReplayResult {
        deterministic,
        replayed_tool_calls,
        recorded_tool_calls,
        replayed_iterations,
        recorded_iterations,
        efficiency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::{Conversation, Message, TokenUsage};

    fn direct_answer_response(text: &str) -> InferenceResponse {
        InferenceResponse {
            message: Message::assistant(text),
            usage: TokenUsage::new(10, 5),
            model: "replay-mock".into(),
            metadata: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_replay_direct_answer_is_deterministic() {
        // A trajectory: one direct-answer response, no tool calls.
        let trajectory = Trajectory {
            input: "What is 2+2?".into(),
            responses: vec![direct_answer_response("4")],
            recorded_tool_calls: vec![],
            recorded_iterations: 1,
        };

        let result = replay_trajectory_with(trajectory).await.expect("replay ok");
        assert!(result.deterministic, "direct-answer replay must be deterministic");
        assert!(result.tool_calls_match());
        assert!(result.replayed_tool_calls.is_empty());
    }

    #[test]
    fn test_trajectory_round_trip() {
        let t = Trajectory {
            input: "hello".into(),
            responses: vec![direct_answer_response("hi")],
            recorded_tool_calls: vec!["grep".into()],
            recorded_iterations: 2,
        };
        let tmp = std::env::temp_dir().join("oneai_replay_roundtrip.json");
        t.save(&tmp).expect("save");
        let loaded = Trajectory::load(&tmp).expect("load");
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(loaded.input, t.input);
        assert_eq!(loaded.recorded_tool_calls, t.recorded_tool_calls);
        assert_eq!(loaded.recorded_iterations, t.recorded_iterations);
        assert_eq!(loaded.responses.len(), 1);
    }

    #[tokio::test]
    async fn test_replay_provider_exhausts_past_recording() {
        let provider = ReplayProvider::new(
            vec![direct_answer_response("only")],
            ModelConfig::default(),
        );
        // First call OK.
        let _ = provider.infer(InferenceRequest {
            conversation: Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        }).await.expect("first replay call");
        // Second call must error — recording exhausted.
        let second = provider.infer(InferenceRequest {
            conversation: Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: std::collections::HashMap::new(),
        }).await;
        assert!(second.is_err(), "ReplayProvider must error when exhausted");
    }
}
