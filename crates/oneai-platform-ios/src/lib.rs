//! # OneAI Platform — iOS
//!
//! iOS platform adapter for the OneAI framework. Provides a `PlatformInteractionGate`
//! (`IOSInteractionGate`) for a non-bus uniffi app: low-risk tool-approval requests
//! auto-proceed, the rest land on a channel the Swift side polls via
//! `IOSInteractionBridge` and resolves with `UIAlertController`.
//!
//! P4 removed the C-callback poll layer — the in-process 3-symbol bus pump
//! (`oneai_submit_directive` / `oneai_poll_yield` / `oneai_shutdown`) resolves
//! approvals as `EngineYield::ApprovalRequest` ↔ `Directive::Approve`, so the
//! bus path no longer needs this gate. It stays as a fallback shell for a
//! non-bus app; native bus consumers see `approval_request` yields off the pump.

mod gate;

use oneai_core::RiskLevel;

pub use gate::{IOSInteractionBridge, IOSInteractionGate};

/// Factory for creating iOS interaction gates.
pub struct IOSInteractionGateFactory;

impl IOSInteractionGateFactory {
    /// Create an iOS interaction gate with an auto-proceed threshold.
    pub fn create(
        buffer_size: usize,
        threshold: RiskLevel,
    ) -> (IOSInteractionGate, IOSInteractionBridge) {
        IOSInteractionGate::new(buffer_size, threshold)
    }

    /// Create a gate where tool-approval requests go through the channel.
    pub fn create_manual_only(buffer_size: usize) -> (IOSInteractionGate, IOSInteractionBridge) {
        IOSInteractionGate::new_manual_only(buffer_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::InteractionGate;
    use oneai_core::{ApprovalRequest, InteractionRequest, InteractionResponse};

    #[tokio::test]
    async fn test_ios_interaction_gate_auto_proceed_low_risk() {
        let (gate, _bridge) = IOSInteractionGateFactory::create(16, RiskLevel::Medium);

        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            permission_level: None,
            justification: "Simple calculation".to_string(),
        };

        let response = gate
            .request(InteractionRequest::ToolApproval { approval: request })
            .await
            .unwrap();
        assert!(matches!(response, InteractionResponse::Proceed));
    }
}
