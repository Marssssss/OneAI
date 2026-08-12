//! `BusInteractionGate` — an [`InteractionGate`] whose decision points route
//! through the [`EngineBus`]'s approval round-trip. For bus consumers this
//! replaces `ChannelInteractionGate`'s ad-hoc mpsc + per-request oneshot with
//! the unified `EngineYield::ApprovalRequest` ↔ `Directive::Approve` pair on
//! the bus's two channels.
//!
//! Only the human-facing decision points are enabled (`ToolApproval`,
//! `PlanDecision`, `PlanReview`, `NetworkApproval`, `McpElicitation`).
//! `PreInfer`/`PostInfer` are programmatic rewrite hooks, not user prompts —
//! disabled so the loop short-circuits them to `Proceed` with zero latency
//! (the same configuration a TUI applies to `ChannelInteractionGate`).

use std::sync::Arc;

use async_trait::async_trait;
use oneai_bus::{BusError, EngineBus};
use oneai_core::error::{InteractionError, OneAIError};
use oneai_core::traits::InteractionGate;
use oneai_core::{InteractionPoint, InteractionRequest, InteractionResponse};

/// An interaction gate backed by an [`EngineBus`]. Construct via
/// [`BusInteractionGate::new`] and wire as the `App`'s `interaction_gate`
/// (`AppBuilder::engine_bus`).
pub struct BusInteractionGate {
    bus: Arc<dyn EngineBus>,
}

impl BusInteractionGate {
    pub fn new(bus: Arc<dyn EngineBus>) -> Self {
        Self { bus }
    }
}

#[async_trait]
impl InteractionGate for BusInteractionGate {
    async fn request(
        &self,
        req: InteractionRequest,
    ) -> oneai_core::error::Result<InteractionResponse> {
        self.bus.request_approval(req).await.map_err(|e| match e {
            BusError::Closed => OneAIError::Interaction(InteractionError::ChannelDropped),
            // Only Closed arises from request_approval; map any other
            // defensively to ChannelDropped (the bus couldn't deliver).
            _other => OneAIError::Interaction(InteractionError::ChannelDropped),
        })
    }

    fn enabled(&self, point: InteractionPoint) -> bool {
        // Human-facing points only; PreInfer/PostInfer are programmatic.
        !matches!(
            point,
            InteractionPoint::PreInfer | InteractionPoint::PostInfer
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_bus::{EngineYield, InProcessBus};

    fn make_bus() -> (Arc<InProcessBus>, Arc<dyn EngineBus>) {
        let (bus, _rx) = InProcessBus::new();
        let arc = Arc::new(bus);
        (arc.clone(), arc as Arc<dyn EngineBus>)
    }

    #[tokio::test]
    async fn disabled_points_short_circuit() {
        let (_, bus) = make_bus();
        let gate = BusInteractionGate::new(bus);
        assert!(!gate.enabled(InteractionPoint::PreInfer));
        assert!(!gate.enabled(InteractionPoint::PostInfer));
        assert!(gate.enabled(InteractionPoint::ToolApproval));
        assert!(gate.enabled(InteractionPoint::PlanDecision));
        assert!(gate.enabled(InteractionPoint::PlanReview));
        assert!(gate.enabled(InteractionPoint::NetworkApproval));
    }

    #[tokio::test]
    async fn approval_round_trips_through_bus() {
        let (bus, engine_bus) = make_bus();
        let gate = BusInteractionGate::new(engine_bus);
        let mut sub = bus.subscribe_yields();

        // Engine asks for approval (blocks).
        let bus_clone = bus.clone();
        let task = tokio::spawn(async move {
            gate.request(InteractionRequest::NetworkApproval {
                host: "evil.example".to_string(),
                requested_by: "shell".to_string(),
            })
            .await
        });

        // Frontend harvests request_id and approves.
        let request_id = match sub.recv().await.unwrap() {
            EngineYield::ApprovalRequest { request_id, .. } => request_id,
            other => panic!("expected ApprovalRequest, got {other:?}"),
        };
        bus_clone
            .submit(oneai_bus::Directive::Approve {
                request_id,
                response: InteractionResponse::Abort {
                    reason: "user denied".to_string(),
                },
            })
            .await
            .unwrap();

        let resp = task.await.unwrap().unwrap();
        assert!(matches!(resp, InteractionResponse::Abort { .. }));
    }
}
