//! Interaction gate — the unified "agent loop suspends → asks the application
//! layer → resumes with a reply" surface for every decision point.
//!
//! Replaces the split between `ApprovalGate` (tool approve/deny only) and the
//! dead PreInfer/PostInfer `LifecycleHook` interactive path. Implementations:
//! - [`NoopInteractionGate`]: every point disabled — the zero-latency default.
//! - [`ChannelInteractionGate`]: mpsc+oneshot bridge to an external UI thread,
//!   configurable per-point via [`InteractionGateConfig`].
//! - [`ThresholdInteractionGate`]: low-risk tools auto-proceed, the rest go to
//!   the channel.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use oneai_core::{
    ApprovalRequest, InteractionPoint, InteractionRequest, InteractionResponse, RiskLevel,
};
use oneai_core::error::{InteractionError, OneAIError, Result};
use oneai_core::traits::InteractionGate;

// ─── NoopInteractionGate ────────────────────────────────────────────────────

/// An interaction gate that disables every decision point.
///
/// `enabled()` returns `false` for all points, so the agent loop short-circuits
/// each interaction block without taking a lock or sending on a channel — zero
/// latency. This is the moral equivalent of `AutoApprovalGate` but covering the
/// full decision surface (PreInfer/PostInfer/PlanDecision/PlanReview too), and
/// is the safe default for production when no UI is wired up.
///
/// `request()` is unreachable in practice (the loop never calls it because
/// `enabled()` is `false`), but returns `Proceed` defensively.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopInteractionGate;

#[async_trait::async_trait]
impl InteractionGate for NoopInteractionGate {
    async fn request(&self, _req: InteractionRequest) -> Result<InteractionResponse> {
        Ok(InteractionResponse::Proceed)
    }

    fn enabled(&self, _point: InteractionPoint) -> bool {
        false
    }
}

// ─── InteractionGateConfig ──────────────────────────────────────────────────

/// Per-point enablement for [`ChannelInteractionGate`] / [`ThresholdInteractionGate`].
///
/// Defaults to all-`true`. A typical TUI flips `preinfer`/`postinfer` to `false`
/// to avoid interrupting every inference iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionGateConfig {
    /// Call back at PreInfer (before each LLM inference).
    pub preinfer: bool,
    /// Call back at PostInfer (after each LLM inference).
    pub postinfer: bool,
    /// Call back for high-risk tool approval.
    pub tool_approval: bool,
    /// Call back for planning tradeoff decisions.
    pub plan_decision: bool,
    /// Call back for final plan confirmation.
    pub plan_review: bool,
}

impl Default for InteractionGateConfig {
    fn default() -> Self {
        Self {
            preinfer: true,
            postinfer: true,
            tool_approval: true,
            plan_decision: true,
            plan_review: true,
        }
    }
}

impl InteractionGateConfig {
    /// Config with only the planning + tool points enabled (PreInfer/PostInfer
    /// off) — the recommended TUI default.
    pub fn tui_default() -> Self {
        Self {
            preinfer: false,
            postinfer: false,
            tool_approval: true,
            plan_decision: true,
            plan_review: true,
        }
    }

    fn enabled_for(&self, point: InteractionPoint) -> bool {
        match point {
            InteractionPoint::PreInfer => self.preinfer,
            InteractionPoint::PostInfer => self.postinfer,
            InteractionPoint::ToolApproval => self.tool_approval,
            InteractionPoint::PlanDecision => self.plan_decision,
            InteractionPoint::PlanReview => self.plan_review,
            // Future variants default to disabled (loop short-circuits to Proceed).
            _ => false,
        }
    }
}

// ─── InteractionPendingItem ─────────────────────────────────────────────────

/// A pending interaction request paired with the one-shot channel on which the
/// UI handler sends its reply.
///
/// Mirrors `ApprovalPendingItem` but carries the full `InteractionRequest`
/// surface so a single channel can dispatch every decision point.
pub struct InteractionPendingItem {
    /// The interaction request details.
    pub request: InteractionRequest,
    /// The one-shot channel to send the response back.
    pub response_tx: oneshot::Sender<InteractionResponse>,
}

// ─── ChannelInteractionGate ─────────────────────────────────────────────────

/// Channel-based interaction gate for interactive flows.
///
/// Uses an mpsc channel to send interaction requests to an external handler
/// (typically a UI thread) and a one-shot channel per request for the reply.
/// Points not enabled in [`InteractionGateConfig`] short-circuit to `Proceed`
/// without touching the channel.
pub struct ChannelInteractionGate {
    pending_tx: mpsc::Sender<InteractionPendingItem>,
    config: InteractionGateConfig,
}

impl ChannelInteractionGate {
    /// Create a new channel-based gate with the given per-point config.
    ///
    /// Returns the gate (for the agent loop) and a receiver (for the UI handler).
    pub fn with_config(
        buffer_size: usize,
        config: InteractionGateConfig,
    ) -> (Self, mpsc::Receiver<InteractionPendingItem>) {
        let (pending_tx, pending_rx) = mpsc::channel(buffer_size);
        (Self { pending_tx, config }, pending_rx)
    }

    /// Create a new channel-based gate with all points enabled.
    pub fn new(buffer_size: usize) -> (Self, mpsc::Receiver<InteractionPendingItem>) {
        Self::with_config(buffer_size, InteractionGateConfig::default())
    }

    /// Convenience: the request point derived from the request variant.
    fn point_of(req: &InteractionRequest) -> InteractionPoint {
        match req {
            InteractionRequest::PreInfer { .. } => InteractionPoint::PreInfer,
            InteractionRequest::PostInfer { .. } => InteractionPoint::PostInfer,
            InteractionRequest::ToolApproval { .. } => InteractionPoint::ToolApproval,
            InteractionRequest::PlanDecision { .. } => InteractionPoint::PlanDecision,
            InteractionRequest::PlanReview { .. } => InteractionPoint::PlanReview,
            // Future variants: default to a typically-disabled point so the loop
            // short-circuits to Proceed rather than silently blocking.
            _ => InteractionPoint::PreInfer,
        }
    }
}

#[async_trait::async_trait]
impl InteractionGate for ChannelInteractionGate {
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse> {
        let point = Self::point_of(&req);
        if !self.config.enabled_for(point) {
            return Ok(InteractionResponse::Proceed);
        }

        let (response_tx, response_rx) = oneshot::channel();
        self.pending_tx
            .send(InteractionPendingItem {
                request: req,
                response_tx,
            })
            .await
            .map_err(|_| OneAIError::Interaction(InteractionError::ChannelDropped))?;

        response_rx
            .await
            .map_err(|_| OneAIError::Interaction(InteractionError::ChannelDropped))
    }

    fn enabled(&self, point: InteractionPoint) -> bool {
        self.config.enabled_for(point)
    }
}

// ─── ThresholdInteractionGate ───────────────────────────────────────────────

/// Channel-based gate that auto-proceeds low-risk tool calls and forwards
/// everything else (including all non-tool decision points) to the channel.
///
/// The moral equivalent of the deprecated `ChannelApprovalGateWithThreshold`,
/// extended to the full interaction surface.
pub struct ThresholdInteractionGate {
    pending_tx: mpsc::Sender<InteractionPendingItem>,
    config: InteractionGateConfig,
    /// Risk level threshold for auto-proceeding tool approvals.
    /// `None` means all tool approvals go through the channel.
    auto_approve_threshold: Option<RiskLevel>,
}

impl ThresholdInteractionGate {
    /// Create a gate that auto-proceeds tool calls strictly below `threshold`
    /// and forwards the rest to the channel.
    pub fn new(
        buffer_size: usize,
        auto_approve_threshold: RiskLevel,
        config: InteractionGateConfig,
    ) -> (Self, mpsc::Receiver<InteractionPendingItem>) {
        let (pending_tx, pending_rx) = mpsc::channel(buffer_size);
        (
            Self {
                pending_tx,
                config,
                auto_approve_threshold: Some(auto_approve_threshold),
            },
            pending_rx,
        )
    }

    /// Create a gate where all enabled points go through the channel (no
    /// auto-approve).
    pub fn new_manual_only(
        buffer_size: usize,
        config: InteractionGateConfig,
    ) -> (Self, mpsc::Receiver<InteractionPendingItem>) {
        let (pending_tx, pending_rx) = mpsc::channel(buffer_size);
        (
            Self {
                pending_tx,
                config,
                auto_approve_threshold: None,
            },
            pending_rx,
        )
    }

    /// Extract the underlying approval request if this is a tool-approval
    /// request (used for threshold short-circuiting).
    fn approval_of(req: &InteractionRequest) -> Option<&ApprovalRequest> {
        match req {
            InteractionRequest::ToolApproval { approval } => Some(approval),
            _ => None,
        }
    }

    /// Forward to the channel and await the reply.
    async fn forward(
        &self,
        req: InteractionRequest,
    ) -> Result<InteractionResponse> {
        let (response_tx, response_rx) = oneshot::channel();
        self.pending_tx
            .send(InteractionPendingItem {
                request: req,
                response_tx,
            })
            .await
            .map_err(|_| OneAIError::Interaction(InteractionError::ChannelDropped))?;
        response_rx
            .await
            .map_err(|_| OneAIError::Interaction(InteractionError::ChannelDropped))
    }
}

#[async_trait::async_trait]
impl InteractionGate for ThresholdInteractionGate {
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse> {
        let point = ChannelInteractionGate::point_of(&req);
        if !self.config.enabled_for(point) {
            return Ok(InteractionResponse::Proceed);
        }

        // Auto-proceed low-risk tool approvals below the threshold.
        if let Some(threshold) = &self.auto_approve_threshold {
            if let Some(approval) = Self::approval_of(&req) {
                if should_auto_approve(&approval.risk_level, threshold) {
                    tracing::info!(
                        "Auto-proceeding tool '{}' with risk level {:?} (below threshold {:?})",
                        approval.tool_name,
                        approval.risk_level,
                        threshold
                    );
                    return Ok(InteractionResponse::Proceed);
                }
            }
        }

        self.forward(req).await
    }

    fn enabled(&self, point: InteractionPoint) -> bool {
        self.config.enabled_for(point)
    }
}

/// Check if a risk level should be auto-approved given the threshold.
///
/// Auto-approves if the request's risk level is strictly below the threshold.
/// Risk level ordering: Low < Medium < High. Mirrors the legacy
/// `approval::should_auto_approve`.
pub fn should_auto_approve(request_level: &RiskLevel, threshold: &RiskLevel) -> bool {
    match (request_level, threshold) {
        (RiskLevel::Low, RiskLevel::Low) => true,
        (RiskLevel::Low, RiskLevel::Medium) => true,
        (RiskLevel::Low, RiskLevel::High) => true,
        (RiskLevel::Medium, RiskLevel::Low) => false,
        (RiskLevel::Medium, RiskLevel::Medium) => true,
        (RiskLevel::Medium, RiskLevel::High) => true,
        (RiskLevel::High, RiskLevel::Low) => false,
        (RiskLevel::High, RiskLevel::Medium) => false,
        (RiskLevel::High, RiskLevel::High) => true,
    }
}

/// Wrap any interaction gate in an `Arc<dyn InteractionGate>`.
pub fn into_shared<G: InteractionGate + 'static>(gate: G) -> Arc<dyn InteractionGate> {
    Arc::new(gate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::{DecisionOption, PlanStep};

    fn sample_plan_decision() -> InteractionRequest {
        InteractionRequest::PlanDecision {
            decision_id: "d1".to_string(),
            question: "优先速度还是正确性？".to_string(),
            context: "tradeoff with no clear winner".to_string(),
            options: vec![
                DecisionOption {
                    id: "opt_a".to_string(),
                    label: "优先速度".to_string(),
                    description: "faster".to_string(),
                    tradeoffs: "less accurate".to_string(),
                },
                DecisionOption {
                    id: "opt_b".to_string(),
                    label: "优先正确性".to_string(),
                    description: "accurate".to_string(),
                    tradeoffs: "slower".to_string(),
                },
            ],
        }
    }

    fn sample_plan_review() -> InteractionRequest {
        InteractionRequest::PlanReview {
            plan: "do the thing".to_string(),
            steps: vec![PlanStep {
                id: "step_1".to_string(),
                description: "first".to_string(),
                coupled: false,
                depends_on: vec![],
                status: oneai_core::PlanStepStatus::Pending,
                active_form: None,
            }],
        }
    }

    fn sample_tool_approval(risk: RiskLevel) -> InteractionRequest {
        InteractionRequest::ToolApproval {
            approval: ApprovalRequest {
                tool_name: "shell".to_string(),
                args: serde_json::json!({"cmd": "ls"}),
                risk_level: risk,
                permission_level: None,
                justification: "test".to_string(),
            },
        }
    }

    // ─── NoopInteractionGate ─────────────────────────────────────────────

    #[tokio::test]
    async fn noop_disables_every_point() {
        let gate = NoopInteractionGate;
        for p in [
            InteractionPoint::PreInfer,
            InteractionPoint::PostInfer,
            InteractionPoint::ToolApproval,
            InteractionPoint::PlanDecision,
            InteractionPoint::PlanReview,
        ] {
            assert!(!gate.enabled(p), "{:?} should be disabled", p);
        }
        // request() still returns Proceed defensively.
        let r = gate.request(sample_plan_decision()).await.unwrap();
        assert!(matches!(r, InteractionResponse::Proceed));
    }

    // ─── ChannelInteractionGate ──────────────────────────────────────────

    #[tokio::test]
    async fn channel_proceed_roundtrip() {
        let (gate, mut rx) = ChannelInteractionGate::new(4);
        let gate = into_shared(gate);
        let req = sample_plan_decision();
        let req_point = ChannelInteractionGate::point_of(&req);
        assert!(gate.enabled(req_point));

        let g = gate.clone();
        let handle = tokio::spawn(async move { g.request(req).await.unwrap() });

        let item = rx.recv().await.unwrap();
        let _ = item.response_tx.send(InteractionResponse::Choose {
            option_id: "opt_b".to_string(),
        });
        let resp = handle.await.unwrap();
        assert!(matches!(resp, InteractionResponse::Choose { option_id } if option_id == "opt_b"));
    }

    #[tokio::test]
    async fn channel_revise_roundtrip() {
        let (gate, mut rx) =
            ChannelInteractionGate::with_config(4, InteractionGateConfig::default());
        let g = into_shared(gate);
        let handle = tokio::spawn(async move {
            g.request(sample_plan_review()).await.unwrap()
        });
        let item = rx.recv().await.unwrap();
        let _ = item.response_tx.send(InteractionResponse::Revise {
            feedback: "make it cheaper".to_string(),
        });
        let resp = handle.await.unwrap();
        assert!(matches!(resp, InteractionResponse::Revise { feedback } if feedback == "make it cheaper"));
    }

    #[tokio::test]
    async fn channel_disabled_point_short_circuits_without_consuming_channel() {
        // Disable PlanDecision; request must return Proceed without sending.
        let cfg = InteractionGateConfig {
            plan_decision: false,
            ..InteractionGateConfig::default()
        };
        let (gate, mut rx) = ChannelInteractionGate::with_config(4, cfg);
        let g = into_shared(gate);
        assert!(!g.enabled(InteractionPoint::PlanDecision));
        let resp = g.request(sample_plan_decision()).await.unwrap();
        assert!(matches!(resp, InteractionResponse::Proceed));
        // Nothing was enqueued.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn channel_dropped_returns_error() {
        let (gate, rx) = ChannelInteractionGate::new(4);
        let g = into_shared(gate);
        // Drop the receiver (UI handler gone) — send will fail on await.
        drop(rx);
        let err = g.request(sample_plan_review()).await.unwrap_err();
        assert!(matches!(
            err,
            OneAIError::Interaction(InteractionError::ChannelDropped)
        ));
    }

    // ─── ThresholdInteractionGate ────────────────────────────────────────

    #[tokio::test]
    async fn threshold_auto_proceeds_low_risk() {
        let (gate, _rx) = ThresholdInteractionGate::new(
            4,
            RiskLevel::High,
            InteractionGateConfig::default(),
        );
        let g = into_shared(gate);
        // Low-risk tool → auto Proceed, no channel interaction.
        let resp = g.request(sample_tool_approval(RiskLevel::Low)).await.unwrap();
        assert!(matches!(resp, InteractionResponse::Proceed));
    }

    #[tokio::test]
    async fn threshold_forwards_high_risk_to_channel() {
        let (gate, mut rx) = ThresholdInteractionGate::new(
            4,
            RiskLevel::Medium,
            InteractionGateConfig::default(),
        );
        let g = into_shared(gate);
        let handle = tokio::spawn(async move {
            g.request(sample_tool_approval(RiskLevel::High)).await.unwrap()
        });
        let item = rx.recv().await.unwrap();
        let _ = item.response_tx.send(InteractionResponse::Abort {
            reason: "too risky".to_string(),
        });
        let resp = handle.await.unwrap();
        assert!(matches!(resp, InteractionResponse::Abort { reason } if reason == "too risky"));
    }

    #[tokio::test]
    async fn threshold_non_tool_request_ignores_threshold() {
        // PlanDecision isn't a tool approval — threshold must not auto-proceed it.
        let (gate, mut rx) = ThresholdInteractionGate::new(
            4,
            RiskLevel::Low,
            InteractionGateConfig::default(),
        );
        let g = into_shared(gate);
        let handle = tokio::spawn(async move {
            g.request(sample_plan_decision()).await.unwrap()
        });
        let item = rx.recv().await.unwrap();
        let _ = item.response_tx.send(InteractionResponse::Choose {
            option_id: "opt_a".to_string(),
        });
        let resp = handle.await.unwrap();
        assert!(matches!(resp, InteractionResponse::Choose { option_id } if option_id == "opt_a"));
    }

    // ─── InteractionGateConfig ───────────────────────────────────────────

    #[tokio::test]
    async fn config_tui_default_disables_infer_points() {
        let cfg = InteractionGateConfig::tui_default();
        assert!(!cfg.preinfer);
        assert!(!cfg.postinfer);
        assert!(cfg.tool_approval);
        assert!(cfg.plan_decision);
        assert!(cfg.plan_review);
    }
}
