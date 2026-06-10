//! # OneAI Platform — HarmonyOS
//!
//! HarmonyOS platform adapter for the OneAI framework.
//! Provides an approval gate that bridges to HarmonyOS CommonDialog
//! via a C callback mechanism, allowing the ArkTS side to present
//! native dialogs for high-risk tool approval.
//!
//! The bridge uses the same C callback pattern as iOS, since
//! HarmonyOS NAPI (C++ wrapper) can call C functions.

mod gate;
mod callback_bridge;

use oneai_core::RiskLevel;

pub use gate::{HarmonyApprovalGate, HarmonyApprovalBridge};
pub use callback_bridge::CallbackApprovalBridge;

/// Factory for creating HarmonyOS approval gates.
pub struct HarmonyApprovalGateFactory;

impl HarmonyApprovalGateFactory {
    /// Create a HarmonyOS approval gate with auto-approve threshold.
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (HarmonyApprovalGate, HarmonyApprovalBridge) {
        HarmonyApprovalGate::new(buffer_size, threshold)
    }

    /// Create a gate where all requests go through the channel (no auto-approve).
    pub fn create_manual_only(buffer_size: usize) -> (HarmonyApprovalGate, HarmonyApprovalBridge) {
        HarmonyApprovalGate::new_manual_only(buffer_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::ApprovalGate;
    use oneai_core::{ApprovalRequest, ApprovalResponse};

    #[tokio::test]
    async fn test_harmony_approval_gate_auto_approve() {
        let (gate, _bridge) = HarmonyApprovalGateFactory::create(16, RiskLevel::Medium);

        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            justification: "Simple calculation".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        assert!(matches!(response, ApprovalResponse::Approved { .. }));
    }
}