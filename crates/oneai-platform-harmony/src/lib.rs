//! # OneAI Platform — HarmonyOS
//!
//! HarmonyOS platform adapter for the OneAI framework.
//! Provides an interaction gate that bridges to HarmonyOS CommonDialog
//! via a C callback mechanism, allowing the ArkTS side to present
//! native dialogs for high-risk tool approval.
//!
//! The bridge uses the same C callback pattern as iOS, since
//! HarmonyOS NAPI (C++ wrapper) can call C functions.

mod gate;
mod callback_bridge;

use oneai_core::RiskLevel;

pub use gate::{HarmonyInteractionGate, HarmonyInteractionBridge};
pub use callback_bridge::CallbackInteractionBridge;

/// Factory for creating HarmonyOS interaction gates.
pub struct HarmonyInteractionGateFactory;

impl HarmonyInteractionGateFactory {
    /// Create a HarmonyOS interaction gate with an auto-proceed threshold.
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (HarmonyInteractionGate, HarmonyInteractionBridge) {
        HarmonyInteractionGate::new(buffer_size, threshold)
    }

    /// Create a gate where tool-approval requests go through the channel.
    pub fn create_manual_only(buffer_size: usize) -> (HarmonyInteractionGate, HarmonyInteractionBridge) {
        HarmonyInteractionGate::new_manual_only(buffer_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::InteractionGate;
    use oneai_core::{ApprovalRequest, InteractionRequest, InteractionResponse};

    #[tokio::test]
    async fn test_harmony_interaction_gate_auto_proceed_low_risk() {
        let (gate, _bridge) = HarmonyInteractionGateFactory::create(16, RiskLevel::Medium);

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
