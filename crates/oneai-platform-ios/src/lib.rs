//! # OneAI Platform — iOS
//!
//! iOS platform adapter for the OneAI framework.
//! Provides an interaction gate that bridges to iOS UIAlertController
//! via a C callback mechanism, allowing the Swift side to present
//! native dialogs for high-risk tool approval.
//!
//! The bridge uses a simple C ABI: the Swift code registers a callback
//! function pointer at init, which receives JSON-encoded tool-approval
//! request strings and sends responses back via another callback.

mod gate;
mod callback_bridge;

use oneai_core::RiskLevel;

pub use gate::{IOSInteractionGate, IOSInteractionBridge};
pub use callback_bridge::CallbackInteractionBridge;

/// Factory for creating iOS interaction gates.
pub struct IOSInteractionGateFactory;

impl IOSInteractionGateFactory {
    /// Create an iOS interaction gate with an auto-proceed threshold.
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (IOSInteractionGate, IOSInteractionBridge) {
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
