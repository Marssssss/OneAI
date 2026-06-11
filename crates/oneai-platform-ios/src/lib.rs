//! # OneAI Platform — iOS
//!
//! iOS platform adapter for the OneAI framework.
//! Provides an approval gate that bridges to iOS UIAlertController
//! via a C callback mechanism, allowing the Swift side to present
//! native dialogs for high-risk tool approval.
//!
//! The bridge uses a simple C ABI: the Swift code registers a callback
//! function pointer at init, which receives JSON-encoded ApprovalRequest
//! strings and sends responses back via another callback.

mod gate;
mod callback_bridge;

use oneai_core::RiskLevel;

pub use gate::{IOSApprovalGate, IOSApprovalBridge};
pub use callback_bridge::CallbackApprovalBridge;

/// Factory for creating iOS approval gates.
pub struct IOSApprovalGateFactory;

impl IOSApprovalGateFactory {
    /// Create an iOS approval gate with auto-approve threshold.
    pub fn create(buffer_size: usize, threshold: RiskLevel) -> (IOSApprovalGate, IOSApprovalBridge) {
        IOSApprovalGate::new(buffer_size, threshold)
    }

    /// Create a gate where all requests go through the channel (no auto-approve).
    pub fn create_manual_only(buffer_size: usize) -> (IOSApprovalGate, IOSApprovalBridge) {
        IOSApprovalGate::new_manual_only(buffer_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::ApprovalGate;
    use oneai_core::{ApprovalRequest, ApprovalResponse};

    #[tokio::test]
    async fn test_ios_approval_gate_auto_approve() {
        let (gate, _bridge) = IOSApprovalGateFactory::create(16, RiskLevel::Medium);

        let request = ApprovalRequest {
            tool_name: "calculator".to_string(),
            args: serde_json::json!({"expression": "2+2"}),
            risk_level: RiskLevel::Low,
            permission_level: None,
            justification: "Simple calculation".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        assert!(matches!(response, ApprovalResponse::Approved { .. }));
    }
}